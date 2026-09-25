use std::collections::BTreeSet;

use rosetta3::{Program, audit_call_targets};

/// A classic `LC_DYLD_INFO_ONLY` image: `call rel32` -> `__stubs` ->
/// `jmp [rip+disp]` -> `__la_symbol_ptr`, the shape the stub premise is about.
const LS: &str = "/bin/ls";
/// A modern chained-fixups image, to show the result is not specific to one
/// encoding.
const ECHO: &str = "/bin/echo";

macro_rules! skip {
    ($($arg:tt)*) => {{
        eprintln!("skipping {}: {}", module_path!(), format!($($arg)*));
        return;
    }};
}

fn load(path: &str) -> Option<Program> {
    if !host_macho(path) {
        return None;
    }
    Some(Program::load(path, &[]).unwrap_or_else(|e| panic!("linking {path} failed: {e}")))
}

fn host_macho(path: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Some(magic) = bytes.get(..4) else {
        return false;
    };
    let magic = u32::from_be_bytes([magic[0], magic[1], magic[2], magic[3]]);
    matches!(
        magic,
        // Thin, 32- and 64-bit, either endianness...
        0xfeed_face | 0xcefa_edfe | 0xfeed_facf | 0xcffa_edfe
        // ...and fat, likewise.
        | 0xcafe_babe | 0xbeba_feca | 0xcafe_babf | 0xbfba_feca
    )
}

/// The audit with a chosen subset of addresses registered.
fn audit(program: &Program, registered: BTreeSet<u64>) -> rosetta3::CallTargetAudit {
    let (text_va, text) = &program.text;
    audit_call_targets(
        text,
        *text_va,
        &program.functions,
        program.text_range(),
        &registered,
    )
}

#[test]
fn every_call_out_of_text_is_interceptable() {
    for path in [LS, ECHO] {
        let Some(program) = load(path) else {
            skip!("{path} not present")
        };

        let registered: BTreeSet<u64> = program.import_addresses().collect();
        let a = audit(&program, registered);

        assert!(
            a.functions_lifted > 0,
            "{path}: nothing lifted, so nothing was checked"
        );
        assert!(
            a.out_of_region > 0,
            "{path}: no calls leave __text, so the test proves nothing"
        );
        assert!(
            a.is_clean(),
            "{path}: {} call target(s) would become UnknownJump/DirectCall: {:x?}",
            a.unresolved.len(),
            a.unresolved.iter().map(|u| u.target).collect::<Vec<_>>()
        );
        assert_eq!(a.intercepted, a.out_of_region);
    }
}

#[test]
fn stub_addresses_are_load_bearing() {
    // The falsification. Register only `slot_va` - what a loader that reported
    // one address per binding would give you - and watch the interception rate
    // collapse, because every `call rel32` targets a `__stubs` entry that is
    // now an unregistered address outside the lifted region.
    let Some(program) = load(LS) else {
        skip!("no host Mach-O at {LS}")
    };

    let with_stubs: BTreeSet<u64> = program.import_addresses().collect();
    let slots_only: BTreeSet<u64> = program
        .imports
        .iter()
        .filter(|(_, site)| !site.is_stub)
        .map(|(&va, _)| va)
        .collect();
    assert!(
        slots_only.len() < with_stubs.len(),
        "{LS} should have stub addresses distinct from its slots"
    );

    let full = audit(&program, with_stubs);
    let crippled = audit(&program, slots_only);

    assert!(full.is_clean());
    assert!(
        !crippled.is_clean(),
        "dropping stub addresses changed nothing, so stub_va is not what makes interception work"
    );
    assert_eq!(
        crippled.intercepted, 0,
        "without stub addresses, not one import call should be intercepted"
    );
    assert_eq!(
        crippled.unresolved.len(),
        full.out_of_region,
        "every call the full audit intercepted should now be uninterceptable"
    );
}

#[test]
fn every_uninterceptable_call_in_the_crippled_audit_is_a_stub() {
    // Sharpens the previous test: the calls that break are not merely *some*
    // addresses, they are exactly the `__stubs` entries. That is the mapping
    // `machish::fixups::stubs` claims to have recovered, checked from the other
    // side - through the lifter's own view of the guest's control flow.
    let Some(program) = load(LS) else {
        skip!("no host Mach-O at {LS}")
    };

    let slots_only: BTreeSet<u64> = program
        .imports
        .iter()
        .filter(|(_, site)| !site.is_stub)
        .map(|(&va, _)| va)
        .collect();
    let crippled = audit(&program, slots_only);

    let stubs: BTreeSet<u64> = program
        .imports
        .iter()
        .filter(|(_, site)| site.is_stub)
        .map(|(&va, _)| va)
        .collect();

    for u in &crippled.unresolved {
        assert!(
            stubs.contains(&u.target),
            "{:#x} is uninterceptable but is not a __stubs entry",
            u.target
        );
    }
}

#[test]
fn linking_reports_a_stub_for_every_lazy_binding() {
    let Some(program) = load(LS) else {
        skip!("no host Mach-O at {LS}")
    };

    let bindings = program.report.bindings().count();
    let with_stub = program
        .report
        .bindings()
        .filter(|b| b.stub_va.is_some())
        .count();
    assert!(with_stub > 0);
    // Not every binding has a stub - a data import like `___stdoutp` is read
    // through the slot and never called - so the claim is about lazy bindings.
    let lazy = program
        .report
        .bindings()
        .filter(|b| b.kind == machish::fixups::BindKind::Lazy)
        .count();
    assert_eq!(
        with_stub, lazy,
        "every lazy binding, and only those, should have a stub"
    );
    assert!(bindings > lazy, "the fixture should also import some data");
}

#[test]
fn the_lifter_config_registers_both_addresses_per_binding() {
    let Some(program) = load(LS) else {
        skip!("no host Mach-O at {LS}")
    };
    let config = program.lifter_config();

    for b in program.report.bindings() {
        assert!(
            config.imports.contains_key(&b.slot_va),
            "{} slot {:#x} was not registered",
            b.symbol,
            b.slot_va
        );
        if let Some(stub) = b.stub_va {
            assert!(
                config.imports.contains_key(&stub),
                "{} stub {:#x} was not registered",
                b.symbol,
                stub
            );
        }
    }
    // And the guest's memory is available for static reads, which is what lets
    // the lifter resolve a RIP-relative constant without a running process.
    assert!(!config.segments.is_empty());
}
