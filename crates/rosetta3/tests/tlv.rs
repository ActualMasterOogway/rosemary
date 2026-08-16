//! `__tlv_bootstrap` must never be reached through the argument marshaller.
//!
//! A `_Thread_local` read is `leaq _x(%rip), %rdi; callq *(%rdi)` — the hottest
//! shape of indirect call a real program has, and the one with no call site for
//! the lifter to match. Every route to it is checked here.

mod common;

use common::{run_guest, run_guest_env};
use rosetta3::abi::Classification;

/// Both spellings, and both must be a fast import: a slow one is the
/// per-call symbol lookup and marshaller walk on every thread-local read.
#[test]
fn the_bootstrap_classifies_fast_under_every_spelling() {
    for name in machish::extensions::tlv::BOOTSTRAP_SYMBOLS {
        assert_eq!(
            rosetta3::abi::classify(0, name),
            Classification::Fast,
            "{name} must be a fast import"
        );
    }
    // The spelling a Mach-O with an extra underscore would carry, which
    // `c_name` strips one of.
    assert_eq!(
        rosetta3::abi::classify(0, "_tlv_bootstrap"),
        Classification::Fast
    );
}

/// The descriptor's `thunk` word — the pointer the guest actually calls — must
/// hold a trampoline once the program is running, so the call is an inline
/// fast import rather than an `ImportedFunctionCall` exit.
#[test]
fn every_descriptor_thunk_points_at_a_trampoline() {
    let Some(path) = common::tls_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let mut program = rosetta3::Program::load(&path, &[]).expect("the fixture links");
    program.install_trampolines();

    let mut seen = 0;
    for image in &program.report.images {
        for d in &image.thread_locals {
            let site = program
                .import_at(d.at)
                .expect("a descriptor's thunk word is an import slot");
            assert!(
                site.symbol.ends_with("tlv_bootstrap"),
                "the first word of a descriptor is the bootstrap, not {}",
                site.symbol
            );
            let mut word = [0u8; 8];
            machish::memory::AddressSpace::read(&program.memory, d.at, &mut word)
                .expect("the descriptor is mapped");
            let thunk = u64::from_le_bytes(word);
            assert!(
                rosetta3::trampoline::contains(thunk),
                "descriptor {:#x} still calls the host address {thunk:#x} directly, \
                 which is the marshaller path",
                d.at
            );
            seen += 1;
        }
    }
    assert!(seen > 0, "the fixture has thread-locals");
}

/// The route that does not go through the slot: a guest holding the *resolved*
/// address and calling it. `dispatch_indirect` answers that from
/// `import_by_target`, and it must find a trampoline there too.
#[test]
fn the_resolved_address_is_trampolined_too() {
    let Some(path) = common::tls_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let program = rosetta3::Program::load(&path, &[]).expect("the fixture links");
    let bootstrap = program
        .report
        .bindings()
        .find(|b| b.symbol.ends_with("tlv_bootstrap"))
        .expect("a program with thread-locals imports a bootstrap");
    let site = program
        .import_by_target(bootstrap.resolved)
        .expect("reachable by the address it resolved to");
    assert!(
        rosetta3::trampoline::for_site(site).is_some(),
        "an indirect call through the raw host address falls back to the marshaller"
    );
}

/// What the multi-route fixture must print. See [`common::TLV_ROUTES_C`] for
/// what each line is the failure of.
const ROUTES: &[&str] = &[
    "main a=11 c=22",
    "worker a=111 c=222",
    "after a=11 c=22",
    "byhand a=11",
    "plug 78 79",
];

fn expect_routes(out: &str, err: &str) {
    for line in ROUTES {
        assert!(
            out.contains(line),
            "missing {line:?} in {out:?} (stderr: {err})"
        );
    }
}

/// Every route, run: a second thread, the thunk word called by hand through a
/// `volatile` pointer, and a `dlopen`ed image's own thread-locals.
#[test]
fn every_route_to_a_thread_local_gets_the_right_answer() {
    let Some(path) = common::tlv_routes_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);
    assert_eq!(status, Some(0), "stderr: {err}");
    expect_routes(&out, &err);
}

/// And the same answers with the substitution off, which is what makes the
/// fast path an optimisation rather than a behaviour change.
#[test]
fn the_answers_are_the_same_without_trampolines() {
    let Some(path) = common::tlv_routes_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest_env(&[("ROSETTA3_TRAMPOLINES", "0")], &path, &[]);
    assert_eq!(status, Some(0), "stderr: {err}");
    expect_routes(&out, &err);
}

/// The one that says the fast path was actually taken.
///
/// Without it the pair above would pass just as happily with every access
/// serviced by the marshaller — which is exactly the regression this file
/// exists to catch, since it is invisible in the output and shows up only as
/// the program being slow. `reached` counts trampolines the guest entered, and
/// a `_Thread_local` program has no other reason to enter one.
#[test]
fn the_trampoline_is_the_path_a_thread_local_read_took() {
    let Some(path) = common::tls_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (_, _, err) = run_guest_env(&[("ROSETTA3_STATS", "1")], &path, &[]);
    let line = err
        .lines()
        .find(|l| l.contains("trampoline(s)"))
        .unwrap_or_else(|| panic!("no trampoline stats in: {err}"));
    let words: Vec<&str> = line.split_whitespace().collect();
    let reached: u64 = words
        .windows(2)
        .find(|w| w[1] == "reached")
        .and_then(|w| w[0].parse().ok())
        .unwrap_or_else(|| panic!("no reached count in: {line}"));
    assert!(
        reached >= 1,
        "the bootstrap was never reached through a trampoline: {line}"
    );
}
