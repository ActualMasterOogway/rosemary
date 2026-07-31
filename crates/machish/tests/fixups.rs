#![cfg(all(feature = "std", feature = "arch-x86_64"))]
//! M1.4/M1.5/M1.6 acceptance: decoding fixups, recovering stub addresses, and
//! binding into an address space.
//!
//! The two claims worth stating outright, because they are the ones the rest of
//! milestone 1 rests on:
//!
//! 1. **`stub_va` is real.** [`stub_addresses_actually_jump_through_their_slots`]
//!    decodes the six bytes at each recovered stub as `ff 25 <rel32>` and checks
//!    the RIP-relative target equals the slot it was paired with. That proves
//!    the pairing rather than asserting it by construction, and it is the first
//!    falsifiable test of the premise the microlift integration depends on.
//! 2. **The two encodings agree.** [`both_encodings_decode_to_the_same_stream`]
//!    builds one `.c` twice and diffs the normalized output. They are
//!    independent expressions of one intent, so agreement is evidence no single
//!    decoder test can give.

mod common;

use std::collections::BTreeMap;

use machish::fixups::{decode_image, BindKind, Binder, Fixup, LibraryRef, StubMap};
use machish::image::{map_segments, MachObject};
use machish::memory::{AddressSpace, BufferSpace};
use machish::report::BindingRecord;
use machish::resolve::{Resolved, SymbolProvider, SyntheticDylib};
use machish::spec::arch::X86_64;

const LS: &str = "/bin/ls";
const ECHO: &str = "/bin/echo";

/// Source used for the cross-encoding comparison. Deliberately touches several
/// libSystem functions and a couple of globals, so both a bind stream and a
/// rebase stream are non-empty.
const AGREE_SRC: &str = r#"
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

static const char *messages[] = { "alpha", "beta", "gamma" };
int counter = 0;

int main(int argc, char **argv) {
    (void)argv;
    for (int i = 0; i < 3; i++) {
        counter += (int)strlen(messages[i]);
    }
    printf("%d %d\n", counter, argc);
    return counter > 0 ? 0 : 1;
}
"#;

// ------------------------------------------------------------- decoding ----

#[test]
fn dyld_info_decodes_lazy_bindings_into_la_symbol_ptr() {
    let Some(bytes) = common::system_binary(LS) else { skip!("{LS} not present") };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    assert!(obj.dyld_info().is_some(), "{LS} should be an LC_DYLD_INFO_ONLY image");

    let (encoding, fixups) = decode_image(&obj).unwrap();
    assert_eq!(encoding, "dyld_info");

    let la = obj.section("__DATA", "__la_symbol_ptr").expect("no __la_symbol_ptr");
    let lazy: Vec<_> = fixups
        .iter()
        .filter(|f| matches!(f, Fixup::Bind { kind: BindKind::Lazy, .. }))
        .collect();

    // Structural, not literal: one lazy binding per slot, every one inside the
    // section. The count follows from the section size, so a relaid-out `/bin/ls`
    // still passes.
    let slots = la.size / 8;
    assert_eq!(lazy.len() as u64, slots, "expected one lazy bind per __la_symbol_ptr slot");
    for f in &lazy {
        assert!(la.contains(f.address()), "lazy bind at {:#x} outside __la_symbol_ptr", f.address());
    }
}

#[test]
fn bind_ordinals_name_a_real_load_command() {
    let Some(bytes) = common::system_binary(LS) else { skip!("{LS} not present") };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let (_, fixups) = decode_image(&obj).unwrap();

    let deps: Vec<&str> = obj.dependencies().iter().map(|d| d.install_name).collect();
    assert!(!deps.is_empty());

    let mut two_level = 0;
    for f in &fixups {
        let Fixup::Bind { symbol, .. } = f else { continue };
        match symbol.library {
            LibraryRef::InstallName(n) => {
                assert!(deps.contains(&n), "{n} is not one of this image's dependencies");
                two_level += 1;
            }
            LibraryRef::UnknownOrdinal(o) => panic!("bind referenced unknown ordinal {o}"),
            _ => {}
        }
        // `dyld_stub_binder` is the one import a Mach-O spells without a
        // leading underscore, so the invariant is "unchanged from the file",
        // not "starts with an underscore".
        assert!(
            symbol.name.starts_with('_') || symbol.name == "dyld_stub_binder",
            "{} is not a name this image could have written",
            symbol.name
        );
    }
    assert!(two_level > 0, "a two-level image should have two-level bindings");
}

#[test]
fn chained_fixups_decode_into_the_same_shape() {
    let Some(bytes) = common::system_binary(ECHO) else { skip!("{ECHO} not present") };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let Some(_) = obj.chained_fixups() else { skip!("{ECHO} is not a chained-fixups image") };

    let (encoding, fixups) = decode_image(&obj).unwrap();
    assert_eq!(encoding, "chained");
    assert!(!fixups.is_empty());

    let base = obj.preferred_base().unwrap();
    let end = base + obj.vm_span().unwrap();
    for f in &fixups {
        assert!(
            (base..end).contains(&f.address()),
            "fixup at {:#x} outside the image",
            f.address()
        );
        // A chained rebase must carry its target: the packed word in the slot
        // is not a pointer, so `Rebase` would be silently wrong here.
        assert!(!matches!(f, Fixup::Rebase { .. }), "chained decoder emitted a stored-value rebase");
    }
}

#[test]
fn arm64e_chained_pointers_are_refused_specifically() {
    // The arm64e slice of a system binary is the only easy source of a
    // PAC-formatted chain. Parsing it as arm64 must fail at the pointer format
    // rather than decode into plausible nonsense.
    use machish::spec::arch::Arm64;
    let Some(bytes) = common::system_binary(LS) else { skip!("{LS} not present") };
    let Ok(obj) = MachObject::parse_any(&bytes, Arm64) else { skip!("no arm64 slice") };
    if obj.chained_fixups().is_none() {
        skip!("arm64 slice does not use chained fixups");
    }

    match decode_image(&obj) {
        Err(machish::Error::UnknownFixup { format, .. }) => {
            assert!(format.contains("arm64e"), "wrong rejection reason: {format}");
        }
        Err(other) => panic!("expected a specific arm64e rejection, got {other}"),
        Ok(_) => skip!("this arm64 slice is not arm64e"),
    }
}

#[test]
fn both_encodings_decode_to_the_same_stream() {
    let Some(chained) = common::build_x86_64("agree_chained", AGREE_SRC, &[]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let Some(opcodes) = common::build_x86_64("agree_opcodes", AGREE_SRC, &["-Wl,-no_fixup_chains"])
    else {
        skip!("cc -arch x86_64 unavailable")
    };

    let a = MachObject::parse_any(&chained, X86_64).unwrap();
    let b = MachObject::parse_any(&opcodes, X86_64).unwrap();
    assert!(a.chained_fixups().is_some(), "expected chained fixups");
    assert!(b.dyld_info().is_some(), "expected LC_DYLD_INFO_ONLY");

    let (_, fa) = decode_image(&a).unwrap();
    let (_, fb) = decode_image(&b).unwrap();

    // Compare by (symbol, library) rather than by address: the two linker
    // invocations lay the image out differently, so slot addresses legitimately
    // differ while the *set of imports* must not.
    let binds = |obj: &MachObject<'_, X86_64>, fixups: &[Fixup<'_>]| -> BTreeMap<String, String> {
        let _ = obj;
        fixups
            .iter()
            .filter_map(|f| match f {
                // `dyld_stub_binder` is the one legitimate difference between
                // the encodings, and excluding it is not weakening the test.
                // The opcode encoding needs it because `__stub_helper` calls
                // into dyld to resolve a lazy slot on first use; chained fixups
                // have no `__stub_helper` at all, so the import genuinely does
                // not exist in that image rather than being missed by us.
                Fixup::Bind { symbol, .. } if symbol.name != "dyld_stub_binder" => Some((
                    symbol.name.to_string(),
                    symbol.library.install_name().unwrap_or("<flat>").to_string(),
                )),
                _ => None,
            })
            .collect()
    };

    let ba = binds(&a, &fa);
    let bb = binds(&b, &fb);
    assert!(!ba.is_empty(), "no imports decoded from the chained image");
    assert_eq!(ba, bb, "the two encodings disagree about this image's imports");
}

// ------------------------------------------------------------ stub_va -------

#[test]
fn every_lazy_binding_carries_a_stub() {
    let Some(bytes) = common::system_binary(LS) else { skip!("{LS} not present") };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let stubs = StubMap::build(&obj).unwrap();
    let (_, fixups) = decode_image(&obj).unwrap();

    let stub_sect = obj.section("__TEXT", "__stubs").expect("no __stubs section");
    assert_eq!(stub_sect.reserved2, 6, "x86-64 stubs are six bytes");

    let mut checked = 0;
    for f in &fixups {
        let Fixup::Bind { at, kind: BindKind::Lazy, .. } = f else { continue };
        let stub = stubs.stub_for(*at).unwrap_or_else(|| panic!("no stub for slot {at:#x}"));
        assert!(stub_sect.contains(stub), "stub {stub:#x} outside __stubs");
        assert_eq!(
            (stub - stub_sect.addr) % 6,
            0,
            "stub {stub:#x} is not on the six-byte stride"
        );
        checked += 1;
    }
    assert!(checked > 0, "no lazy bindings to check");
}

#[test]
fn stub_addresses_actually_jump_through_their_slots() {
    // The claim being tested: `stub_va` is the address whose `jmp` lands on
    // `slot_va`. Decoding the instruction proves the pairing; comparing counts
    // or ranges would only show the two tables are the same size.
    let Some(bytes) = common::system_binary(LS) else { skip!("{LS} not present") };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let stubs = StubMap::build(&obj).unwrap();
    assert!(!stubs.is_empty());

    let mut verified = 0;
    for (slot, stub) in stubs.iter() {
        let off = obj.vmaddr_to_off(stub).expect("stub has no file bytes");
        let code = &obj.bytes()[off..off + 6];
        // `ff 25 <rel32>` is `jmp qword [rip + rel32]`.
        assert_eq!(&code[..2], &[0xff, 0x25], "stub at {stub:#x} is not an indirect jmp");
        let rel = i32::from_le_bytes([code[2], code[3], code[4], code[5]]);
        let target = (stub + 6).wrapping_add(rel as i64 as u64);
        assert_eq!(
            target, slot,
            "stub {stub:#x} jumps through {target:#x}, but was paired with slot {slot:#x}"
        );
        verified += 1;
    }
    assert!(verified >= 80, "expected many stubs in {LS}, verified only {verified}");
}

// ------------------------------------------------------------- binding ------

/// A provider that answers everything with a distinct, recognisable address, so
/// a slot's contents identify which symbol was written there.
struct Counting {
    inner: SyntheticDylib,
}

impl Counting {
    fn over(obj: &MachObject<'_, X86_64>, fixups: &[Fixup<'_>]) -> (Self, BTreeMap<String, u64>) {
        let mut lib = SyntheticDylib::new("counting");
        lib.flat(true);
        let mut expected = BTreeMap::new();
        let mut next = 0x7000_0000_0000u64;
        for f in fixups {
            let Fixup::Bind { symbol, .. } = f else { continue };
            if expected.contains_key(symbol.name) {
                continue;
            }
            lib.define(symbol.name, Resolved::function(next).with_tag(expected.len() as u64));
            expected.insert(symbol.name.to_string(), next);
            next += 0x1000;
        }
        let _ = obj;
        (Counting { inner: lib }, expected)
    }
}

impl SymbolProvider for Counting {
    fn provides(&self, install_name: &str) -> bool {
        self.inner.provides(install_name)
    }
    fn resolve(&self, sym: &machish::fixups::SymbolRef<'_>) -> Option<Resolved> {
        self.inner.resolve(sym)
    }
}

#[test]
fn binding_writes_the_expected_pointer_into_every_slot() {
    let Some(bytes) = common::system_binary(LS) else { skip!("{LS} not present") };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    let mut mem = BufferSpace::new(0x1_0000_0000);
    let mapped = map_segments(&obj, &mut mem).unwrap();
    let (_, fixups) = decode_image(&obj).unwrap();
    let stubs = StubMap::build(&obj).unwrap();

    let (provider, expected) = Counting::over(&obj, &fixups);
    let providers: [&dyn SymbolProvider; 1] = [&provider];
    let binder = Binder { slide: mapped.slide, providers: &providers, self_exports: None, main_executable: None };

    let outcome = binder.apply(&fixups, &mut mem, &stubs).unwrap();
    assert!(outcome.missing.is_empty(), "unexpectedly missing: {:?}", outcome.missing);
    assert_eq!(outcome.bindings.len(), fixups.iter().filter(|f| f.is_bind()).count());

    // Slot by slot: read back what was written and check it is the address the
    // provider promised for that symbol.
    for b in &outcome.bindings {
        let mut buf = [0u8; 8];
        mem.read(b.slot_va, &mut buf).unwrap();
        let got = u64::from_le_bytes(buf);
        assert_eq!(got, expected[&b.symbol], "wrong pointer in slot for {}", b.symbol);
        assert_eq!(got, b.resolved, "the record disagrees with memory for {}", b.symbol);
    }
}

#[test]
fn every_binding_record_is_fully_populated() {
    let Some(bytes) = common::system_binary(LS) else { skip!("{LS} not present") };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    let mut mem = BufferSpace::new(0x1_0000_0000);
    let mapped = map_segments(&obj, &mut mem).unwrap();
    let (_, fixups) = decode_image(&obj).unwrap();
    let stubs = StubMap::build(&obj).unwrap();
    let (provider, _) = Counting::over(&obj, &fixups);
    let providers: [&dyn SymbolProvider; 1] = [&provider];

    let outcome = Binder { slide: mapped.slide, providers: &providers, self_exports: None, main_executable: None }
        .apply(&fixups, &mut mem, &stubs)
        .unwrap();

    let stub_sect = obj.section("__TEXT", "__stubs").unwrap();
    let mut lazy = 0;
    for b in &outcome.bindings {
        assert!(b.symbol.starts_with('_') || b.symbol == "dyld_stub_binder");
        assert!(b.is_satisfied());
        if b.kind == BindKind::Lazy {
            lazy += 1;
            let stub = b.stub_va.unwrap_or_else(|| panic!("lazy binding {} has no stub", b.symbol));
            assert!(stub_sect.contains(stub.wrapping_sub(mapped.slide)));
        }
    }
    assert!(lazy > 0);

    // The record set must be usable as a lookup key for interception, i.e. no
    // two bindings may claim the same slot.
    let mut seen = BTreeMap::new();
    for b in &outcome.bindings {
        assert!(seen.insert(b.slot_va, &b.symbol).is_none(), "duplicate slot {:#x}", b.slot_va);
    }
}

#[test]
fn an_unsatisfied_hard_import_is_reported_not_silently_zeroed() {
    let Some(bytes) = common::system_binary(LS) else { skip!("{LS} not present") };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    let mut mem = BufferSpace::new(0x1_0000_0000);
    let mapped = map_segments(&obj, &mut mem).unwrap();
    let (_, fixups) = decode_image(&obj).unwrap();

    // No providers at all: everything must land in `missing`, and every slot
    // must read back as zero rather than as leftover file content.
    let outcome = Binder::new(mapped.slide)
        .apply(&fixups, &mut mem, &StubMap::default())
        .unwrap();
    assert!(outcome.bindings.is_empty());
    assert_eq!(outcome.missing.len(), fixups.iter().filter(|f| f.is_bind()).count());

    for m in &outcome.missing {
        let mut buf = [0u8; 8];
        mem.read(m.slot_va, &mut buf).unwrap();
        assert_eq!(u64::from_le_bytes(buf), 0, "{} left a stale pointer behind", m.symbol);
    }
}

#[test]
fn a_synthetic_dylib_satisfies_a_built_fixtures_imports() {
    // The end-to-end shape of `selfish/tests/aarch64_shim.rs:58-63`: build a
    // binary, bind it against addresses we chose, assert the slots.
    let src = r#"
        #include <string.h>
        size_t f(const char *s) { return strlen(s); }
        int main(void) { return (int)f("hello"); }
    "#;
    let Some(bytes) = common::build_x86_64("bind_synthetic", src, &["-Wl,-no_fixup_chains"]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    let mut mem = BufferSpace::new(0x1_0000_0000);
    let mapped = map_segments(&obj, &mut mem).unwrap();
    let (_, fixups) = decode_image(&obj).unwrap();
    let stubs = StubMap::build(&obj).unwrap();

    let mut lib = SyntheticDylib::new("shims");
    lib.flat(true);
    for f in &fixups {
        if let Fixup::Bind { symbol, .. } = f {
            lib.define(symbol.name, Resolved::function(0xdead_0000).with_tag(7));
        }
    }
    let providers: [&dyn SymbolProvider; 1] = [&lib];
    let outcome = Binder { slide: mapped.slide, providers: &providers, self_exports: None, main_executable: None }
        .apply(&fixups, &mut mem, &stubs)
        .unwrap();

    assert!(outcome.missing.is_empty());
    let strlen: Vec<&BindingRecord> =
        outcome.bindings.iter().filter(|b| b.symbol == "_strlen").collect();
    assert!(!strlen.is_empty(), "the fixture must import _strlen");
    for b in strlen {
        assert_eq!(b.resolved, 0xdead_0000);
        assert_eq!(b.tag, 7, "the provider's tag must survive into the record");
        let mut buf = [0u8; 8];
        mem.read(b.slot_va, &mut buf).unwrap();
        assert_eq!(u64::from_le_bytes(buf), 0xdead_0000);
    }
}
