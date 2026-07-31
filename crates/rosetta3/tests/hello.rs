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

use rosetta3::abi::{self, Classification};
use rosetta3::{ImportPolicy, Program};

fn load(path: &str) -> Program {
    Program::load(path, &[]).unwrap_or_else(|e| panic!("linking {path} failed: {e}"))
}

#[test]
fn every_import_of_the_fixture_is_classified() {
    let Some(path) = common::hello() else { skip!("cannot build x86_64 fixtures") };
    let program = load(&path);

    let classified = program.classify_imports();
    assert!(!classified.is_empty(), "a program that imports nothing proves nothing");

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
    let Some(path) = common::hello() else { skip!("cannot build x86_64 fixtures") };
    let program = load(&path);
    let classified = program.classify_imports();

    let has = |want: fn(&Classification) -> bool| classified.values().any(want);
    assert!(has(|c| matches!(c, Classification::Fast)), "no import is fast-callable");
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
    assert!(unknown.is_empty(), "unclassified import(s) in /bin/echo: {unknown:?}");
}

#[test]
fn a_known_signature_is_inlined_and_a_variadic_is_not() {
    // The promotion itself: microlift is told the ABI for `_puts` and told
    // nothing but the address for `_printf`, because the second cannot be
    // forwarded without parsing its format string.
    let Some(path) = common::hello() else { skip!("cannot build x86_64 fixtures") };
    let program = load(&path);
    let config = program.lifter_config();

    let mut fast = 0usize;
    for (va, site) in &program.imports {
        let entry = config.imports.get(va).expect("every import address is registered");
        match abi::classify(site.tag, &site.symbol) {
            Classification::Fast => {
                let import = entry.as_ref().unwrap_or_else(|| {
                    panic!("{} is fast-callable but was registered slow", site.symbol)
                });
                assert_eq!(import.symbol.value as usize as u64, site.resolved);
                fast += 1;
            }
            _ => assert!(
                entry.is_none(),
                "{} has no usable signature but was registered fast",
                site.symbol
            ),
        }
    }
    assert!(fast > 0, "nothing was promoted, so the fast path is untested");
}

#[test]
fn the_slow_policy_registers_the_same_addresses_and_no_metadata() {
    // Fast is an optimisation of slow: the two configurations must differ in
    // what microlift is *told*, and not in which call sites it intercepts —
    // that is what makes promotion safe to turn on or off.
    let Some(path) = common::hello() else { skip!("cannot build x86_64 fixtures") };
    let program = load(&path);

    let fast = program.lifter_config_with(ImportPolicy::FastWhenKnown);
    let slow = program.lifter_config_with(ImportPolicy::AlwaysSlow);

    let addrs = |c: &microlift::core::LifterConfig| {
        let mut v: Vec<u64> = c.imports.keys().copied().collect();
        v.sort_unstable();
        v
    };
    assert_eq!(addrs(&fast), addrs(&slow));
    assert!(slow.imports.values().all(Option::is_none));
    assert!(fast.imports.values().any(Option::is_some));
}

#[test]
fn servicing_an_import_exit_calls_the_host_function() {
    // The slow path end to end, driven the way the M3 loop will drive it: an
    // exit names an address, the program looks it up, and the host function
    // runs. Everything before this test checks a piece of that in isolation.
    let Some(path) = common::hello() else { skip!("cannot build x86_64 fixtures") };
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

    program.service_import_call(&mut ctx).expect("strlen is callable");

    assert_eq!(ctx.rax, 12);
    // The call was intercepted before it executed, so neither the resume point
    // nor the stack is the runtime's to adjust.
    assert_eq!(ctx.rip, 0x1234);
    assert_eq!(ctx.rsp, 0);
}

#[test]
fn servicing_an_unregistered_address_fails_rather_than_calling_it() {
    let Some(path) = common::hello() else { skip!("cannot build x86_64 fixtures") };
    let mut program = load(&path);

    let mut ctx = RegisterContext::new();
    ctx.imported_function_va = 0xdead_beef;
    let err = program.service_import_call(&mut ctx).expect_err("that address is not an import");
    assert!(err.to_string().contains("deadbeef"), "the diagnostic should name the address");
}

#[test]
fn the_entry_function_still_lifts_with_imports_promoted() {
    // Promotion changes the IR the lifter emits for a call site, so the fixture
    // must still lift with it on — otherwise the acceptance above would be
    // describing a program the JIT cannot translate.
    let Some(path) = common::hello() else { skip!("cannot build x86_64 fixtures") };
    let program = load(&path);
    let entry = program.entry().expect("an executable has an entry point");
    let (start, bytes) = program.lift_region(entry).expect("entry is inside a known function");

    for policy in [ImportPolicy::FastWhenKnown, ImportPolicy::AlwaysSlow] {
        let config = program.lifter_config_with(policy);
        microlift::core::lift_to_function::<microlift::arch::x86_64::X86_64>(bytes, start, config)
            .unwrap_or_else(|e| panic!("{policy:?}: lifting the entry function failed: {e}"));
    }
}
