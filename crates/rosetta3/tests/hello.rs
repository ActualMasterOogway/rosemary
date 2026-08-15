//! Milestone 2's acceptance gate.
//!
//! The claim: for a program we compiled and can therefore describe completely,
//! **every import is classified** — each one is either callable with a known
//! signature, serviced by a shim, or refused by name with a reason. Nothing is
//! unknown, because an unknown symbol is precisely the case that becomes a
//! plausible-looking wrong value in a register instead of a diagnostic.
//!
//! The fixture is a hello world rather than `/bin/ls` (which milestone 1 used)
//! for a reason unrelated to the ABI: `/bin/ls` embeds a jump table in
//! `__text`, and two of its fifty functions do not lift past it. That is a
//! microlift gap, and putting it inside an ABI test would mean a red suite here
//! whenever the recompiler is behind. `/bin/echo` is checked alongside as the
//! system-binary case.

mod common;

use microlift::arch::x86_64::context::RegisterContext;
use microlift::core::bridge::ImportKind;

use rosetta3::abi::{self, Classification};
use rosetta3::{ImportPolicy, Program};

fn load(path: &str) -> Program {
    Program::load(path, &[]).unwrap_or_else(|e| panic!("linking {path} failed: {e}"))
}

#[test]
fn every_import_of_the_fixture_is_classified() {
    let Some(path) = common::hello() else {
        skip!("cannot build x86_64 fixtures")
    };
    let program = load(&path);

    let classified = program.classify_imports();
    assert!(
        !classified.is_empty(),
        "a program that imports nothing proves nothing"
    );

    let unknown: Vec<&str> = classified
        .iter()
        .filter(|(_, c)| **c == Classification::Unknown)
        .map(|(s, _)| *s)
        .collect();
    assert!(unknown.is_empty(), "unclassified import(s): {unknown:?}");
}

#[test]
fn the_fixture_exercises_all_three_outcomes() {
    // Guards the test above against passing vacuously. A program whose every
    // import happened to be a simple `int f(void)` would classify perfectly and
    // say nothing about variadics or refusals.
    let Some(path) = common::hello() else {
        skip!("cannot build x86_64 fixtures")
    };
    let program = load(&path);
    let classified = program.classify_imports();

    let has = |want: fn(&Classification) -> bool| classified.values().any(want);
    assert!(
        has(|c| matches!(c, Classification::Fast)),
        "no import is fast-callable"
    );
    assert!(
        has(|c| matches!(c, Classification::Slow)),
        "no import needs a VM exit, so the variadic path is untested"
    );
    assert!(
        has(|c| matches!(c, Classification::Refused(_))),
        "nothing is refused, so the refusal path is untested"
    );

    // Named, because these are the specific claims: `printf` cannot be
    // forwarded generically, and the stack guard is data.
    assert_eq!(classified.get("_printf"), Some(&Classification::Slow));
    assert!(matches!(
        classified.get("___stack_chk_guard"),
        Some(Classification::Refused(_)) | None
    ));
}

#[test]
fn every_import_of_bin_echo_is_classified() {
    if !std::path::Path::new("/bin/echo").exists() {
        skip!("/bin/echo not present");
    }
    let program = load("/bin/echo");
    let unknown: Vec<&str> = program
        .classify_imports()
        .iter()
        .filter(|(_, c)| **c == Classification::Unknown)
        .map(|(s, _)| *s)
        .collect();
    assert!(
        unknown.is_empty(),
        "unclassified import(s) in /bin/echo: {unknown:?}"
    );
}

#[test]
fn a_known_signature_is_inlined_and_a_variadic_is_not() {
    // The promotion itself: microlift is told the ABI for `_puts` and told
    // nothing but the address for `_printf`, because the second cannot be
    // forwarded without parsing its format string.
    let Some(path) = common::hello() else {
        skip!("cannot build x86_64 fixtures")
    };
    let program = load(&path);
    let config = program.lifter_config();

    let mut fast = 0usize;
    for (va, site) in &program.imports {
        let entry = config
            .imports
            .get(va)
            .expect("every import address is registered");
        match abi::classify(site.tag, &site.symbol) {
            Classification::Fast => {
                let ImportKind::Fast(import) = entry else {
                    panic!(
                        "{} is fast-callable but was not registered fast",
                        site.symbol
                    )
                };
                assert_eq!(import.symbol.value as usize as u64, site.resolved);
                fast += 1;
            }
            // Not inlinable, and no longer an exit either: a symbol microlift
            // cannot reproduce the call for is reached through the runtime's
            // own bridge. See `rosetta3::bridge`.
            _ => assert!(
                matches!(entry, ImportKind::Bridge(_)),
                "{} has no usable signature but was not registered as a bridge",
                site.symbol
            ),
        }
    }
    assert!(
        fast > 0,
        "nothing was promoted, so the fast path is untested"
    );
}

#[test]
fn the_slow_policy_registers_the_same_addresses_and_no_metadata() {
    // Fast is an optimisation of slow: the two configurations must differ in
    // what microlift is *told*, and not in which call sites it intercepts —
    // that is what makes promotion safe to turn on or off.
    let Some(path) = common::hello() else {
        skip!("cannot build x86_64 fixtures")
    };
    let program = load(&path);

    let fast = program.lifter_config_with(ImportPolicy::FastWhenKnown);
    let slow = program.lifter_config_with(ImportPolicy::AlwaysSlow);

    let addrs = |c: &microlift::core::LifterConfig| {
        let mut v: Vec<u64> = c.imports.keys().copied().collect();
        v.sort_unstable();
        v
    };
    assert_eq!(addrs(&fast), addrs(&slow));
    // The baseline is exits and nothing else — no inlined calls, no bridges,
    // and none of the lift-time machinery either, or it would not be a baseline.
    assert!(slow.imports.values().all(|k| matches!(k, ImportKind::Slow)));
    assert!(slow.import_registry.is_none());
    assert!(slow.indirect_call_bridge.is_none());

    // Under the fast policy every site is reached without an exit: inlined
    // where the signature allows, bridged where it does not.
    assert!(
        fast.imports
            .values()
            .any(|k| matches!(k, ImportKind::Fast(_)))
    );
    assert!(
        !fast.imports.values().any(|k| matches!(k, ImportKind::Slow)),
        "an import was left on the slow exit path"
    );
    assert!(fast.import_registry.is_some());
    assert!(fast.indirect_call_bridge.is_some());
}

#[test]
fn a_writable_segment_is_not_handed_to_the_lifter_as_constant_data() {
    // The whole point of `LifterConfig::segments` is that microlift may fold a
    // load from one to a constant, so a `__DATA` page described as read-only is
    // a wrong value baked into the code — the guest stores to it at run time and
    // the lift never sees that. Every mapped byte still has to be described, so
    // this checks both halves: writability matches the image's own `initprot`,
    // and the pieces still tile each mapped region exactly.
    let Some(path) = common::hello() else {
        skip!("cannot build x86_64 fixtures")
    };
    let program = load(&path);
    let config = program.lifter_config();

    let mut checked = 0;
    for image in &program.report.images {
        for seg in &image.segments {
            // Zero-fill tails (`__bss`) are not mapped bytes, so only the part
            // of a segment a region actually covers is described at all.
            for va in [seg.vmaddr, seg.vmaddr + seg.vmsize.saturating_sub(1)] {
                let Some(piece) = config.segments.iter().find(|s| s.contains(va)) else {
                    continue;
                };
                assert_eq!(
                    piece.writable, seg.writable,
                    "{} {} at {va:#x} was described as writable={}",
                    image.name, seg.name, piece.writable
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked > 0,
        "no segment was described, so nothing is tested"
    );
    assert!(
        program
            .report
            .images
            .iter()
            .flat_map(|i| &i.segments)
            .any(|s| s.writable),
        "the fixture has no writable segment, so the read-only case is vacuous"
    );

    // No byte lost and none described twice, which is what makes the split
    // above equivalent to handing each region over whole.
    let mut pieces: Vec<(u64, u64)> = config
        .segments
        .iter()
        .map(|s| (s.address, s.address + s.data.len() as u64))
        .collect();
    pieces.sort_unstable();
    for regions in program.memory.regions() {
        let (base, bytes) = regions;
        let end = base + bytes.len() as u64;
        let mut at = base;
        for &(start, stop) in pieces.iter().filter(|p| p.0 >= base && p.1 <= end) {
            assert_eq!(start, at, "a gap or overlap at {at:#x}");
            at = stop;
        }
        assert_eq!(at, end, "region {base:#x} is not fully described");
    }
}

#[test]
fn servicing_an_import_exit_calls_the_host_function() {
    // The slow path end to end, driven the way the M3 loop will drive it: an
    // exit names an address, the program looks it up, and the host function
    // runs. Everything before this test checks a piece of that in isolation.
    let Some(path) = common::hello() else {
        skip!("cannot build x86_64 fixtures")
    };
    let mut program = load(&path);

    let (&va, _) = program
        .imports
        .iter()
        .find(|(_, site)| site.symbol == "_strlen" && !site.is_stub)
        .expect("the fixture calls strlen");

    let text = std::ffi::CString::new("twelve chars").unwrap();
    let mut ctx = RegisterContext::new();
    ctx.imported_function_va = va;
    ctx.rdi = text.as_ptr() as usize as u64;
    ctx.rip = 0x1234;

    program
        .service_import_call(&mut ctx)
        .expect("strlen is callable");

    assert_eq!(ctx.rax, 12);
    // The call was intercepted before it executed, so neither the resume point
    // nor the stack is the runtime's to adjust.
    assert_eq!(ctx.rip, 0x1234);
    assert_eq!(ctx.rsp, 0);
}

#[test]
fn servicing_an_unregistered_address_fails_rather_than_calling_it() {
    let Some(path) = common::hello() else {
        skip!("cannot build x86_64 fixtures")
    };
    let mut program = load(&path);

    let mut ctx = RegisterContext::new();
    ctx.imported_function_va = 0xdead_beef;
    let err = program
        .service_import_call(&mut ctx)
        .expect_err("that address is not an import");
    assert!(
        err.to_string().contains("deadbeef"),
        "the diagnostic should name the address"
    );
}

#[test]
fn the_entry_function_still_lifts_with_imports_promoted() {
    // Promotion changes the IR the lifter emits for a call site, so the fixture
    // must still lift with it on — otherwise the acceptance above would be
    // describing a program the JIT cannot translate.
    let Some(path) = common::hello() else {
        skip!("cannot build x86_64 fixtures")
    };
    let program = load(&path);
    let entry = program.entry().expect("an executable has an entry point");
    let (region, bytes) = program
        .lift_region(entry)
        .expect("entry is inside a known function");
    let start = region.range.start;

    for policy in [ImportPolicy::FastWhenKnown, ImportPolicy::AlwaysSlow] {
        let config = program.lifter_config_with(policy);
        // `lift_to_code`, not `lift_to_function`: under a pinned link ABI a lift
        // is a region body *plus* the entry thunk that calls it, and the
        // single-`Function` entry point refuses rather than dropping one.
        microlift::core::lift_to_code::<microlift::arch::x86_64::X86_64>(&bytes, start, config)
            .unwrap_or_else(|e| panic!("{policy:?}: lifting the entry function failed: {e}"));
    }
}
