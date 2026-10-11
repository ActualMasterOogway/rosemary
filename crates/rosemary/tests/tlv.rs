mod common;

use common::{run_guest, run_guest_env};
use rosemary::abi::Classification;

/// Both spellings, and both must be a fast import: a slow one is the
/// per-call symbol lookup and marshaller walk on every thread-local read.
#[test]
fn the_bootstrap_classifies_fast_under_every_spelling() {
    for name in machish::extensions::tlv::BOOTSTRAP_SYMBOLS {
        assert_eq!(
            rosemary::abi::classify(0, name),
            Classification::Fast,
            "{name} must be a fast import"
        );
    }
    // The spelling a Mach-O with an extra underscore would carry, which
    // `c_name` strips one of.
    assert_eq!(
        rosemary::abi::classify(0, "_tlv_bootstrap"),
        Classification::Fast
    );
}

/// dyld's `tlv_initialize_descriptors`: every descriptor's `thunk` word is
/// `tlv_get_addr` - guest code, so a hit never leaves the JIT - and its `key` word is
/// a Darwin key, so `%gs:[key*8]` is this thread's block.
#[test]
fn every_descriptor_points_at_tlv_get_addr_with_its_image_s_key() {
    let Some(path) = common::tls_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let program = rosemary::Program::load_executable(&path, &[]).expect("the fixture links");
    let registered = rosemary::tls::register(&program.report, rosemary::exec::Engine::Emulated);

    let mut seen = 0;
    for image in &program.report.images {
        for d in &image.thread_locals {
            let mut words = [0u8; 16];
            machish::memory::AddressSpace::read(&program.memory, d.at, &mut words)
                .expect("the descriptor is mapped");
            let thunk = u64::from_le_bytes(words[..8].try_into().unwrap());
            let key = u64::from_le_bytes(words[8..].try_into().unwrap());
            assert!(
                rosemary::tls::contains(thunk),
                "descriptor {:#x} calls {thunk:#x}, not tlv_get_addr",
                d.at
            );
            if darwin::tls::key::addressable() {
                assert!(
                    (u64::from(darwin::tls::key::FIRST_DYNAMIC)..u64::from(darwin::tls::key::END))
                        .contains(&key),
                    "descriptor {:#x} carries {key}, not a Darwin key",
                    d.at
                );
            }
            seen += 1;
        }
    }
    assert!(seen > 0, "the fixture has thread-locals");
    assert_eq!(registered, seen, "every descriptor was rewritten");
}

/// The route that does not go through the slot: a guest holding the *resolved*
/// address and calling it. `dispatch_indirect` answers that from
/// `import_by_target`, and it must find a trampoline there too.
#[test]
fn the_resolved_address_is_trampolined_too() {
    let Some(path) = common::tls_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let program = rosemary::Program::load(&path, &[]).expect("the fixture links");
    let bootstrap = program
        .report
        .bindings()
        .find(|b| b.symbol.ends_with("tlv_bootstrap"))
        .expect("a program with thread-locals imports a bootstrap");
    let site = program
        .import_by_target(bootstrap.resolved)
        .expect("reachable by the address it resolved to");
    assert!(
        rosemary::trampoline::for_site(site).is_some(),
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
    let (status, out, err) = run_guest_env(&[("ROSEMARY_TRAMPOLINES", "0")], &path, &[]);
    assert_eq!(status, Some(0), "stderr: {err}");
    expect_routes(&out, &err);
}

/// Thread-local terminators run as Darwin runs them - per thread, last registered
/// first, while the thread's blocks are still there, and the main thread's at
/// `exit` ahead of `atexit` - and print what the same source built for this host
/// prints.
#[test]
fn thread_local_terminators_run_in_darwin_s_order() {
    let Some(path) = common::tlv_exit_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);
    assert_eq!(status, Some(0), "stderr: {err}");
    let expected = "bye second value=100\nbye first value=100\n\
                    bye second value=101\nbye first value=101\n\
                    sum 500007500000\nbye main value=1000007\natexit\n";
    assert_eq!(out, expected, "stderr: {err}");
    if let Some(native) = common::build_native("tlv_exit_native", common::TLV_EXIT_C)
        && let Some((_, native_out)) = common::run_native(&native)
    {
        assert_eq!(out, native_out, "the host's own Darwin disagrees");
    }
}

/// A million thread-local reads, and the host is asked about almost none of them:
/// after each thread's first touch, `tlv_get_addr` is lifted guest code reached by
/// a linked call.
#[test]
fn a_thread_local_read_stays_in_the_jit() {
    let Some(path) = common::tlv_exit_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (_, _, err) = run_guest_env(&[("ROSEMARY_STATS", "1")], &path, &[]);
    let line = err
        .lines()
        .find(|l| l.contains("indirect call(s) dispatched by the host"))
        .unwrap_or_else(|| panic!("no dispatch stats in: {err}"));
    let dispatched: u64 = line
        .split_whitespace()
        .find_map(|w| w.parse().ok())
        .unwrap_or_else(|| panic!("no count in: {line}"));
    assert!(
        dispatched < 1000,
        "thread-local reads are leaving the JIT: {line}"
    );
}
