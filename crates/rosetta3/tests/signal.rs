//! Milestone 5: signal dispositions, and the kernel calling a guest function.
//!
//! The third distinct caller of a host→guest thunk after `qsort` and
//! `pthread_create`, and the first that arrives with the runtime nowhere nearby:
//! the kernel pushes a frame onto whichever thread it picked and calls the
//! handler. What each test would falsify:
//!
//! - **the handler runs** — installed through both `signal` and `sigaction`, and
//!   counted, so a handler that never fired is a wrong number rather than a
//!   missing line.
//! - **a disposition comes back** — the direction a runtime that only
//!   substituted outbound pointers gets wrong. The guest compares what `signal`
//!   returned against its own function and restores it.
//! - **sentinels are not addresses** — `SIG_IGN` installed as a thunk would run
//!   a handler where the program asked for silence.
//! - **`SA_SIGINFO` is refused by name** — the boundary, for a reason that
//!   belongs to the recompiler rather than to this runtime.
//! - **both architectures agree** — the same source built for arm64 and run
//!   natively prints the same thing, which is what says `struct sigaction`
//!   needs no translation.

mod common;

use common::{SIGNAL_C, build_native, run_guest, run_native, siginfo_fixture, signal_fixture};

const EXPECTED: &str = "\
layout 16 4
previous was default 1
handler saw 30
saved is mine 1
handler saw 30
old was default 1
handler saw 31
queried is mine 1
hits 3
";

#[test]
fn the_kernel_calls_a_guest_handler_through_both_installers() {
    let Some(path) = signal_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, EXPECTED, "stderr: {err}");
}

#[test]
fn a_saved_disposition_is_the_guests_own_function_and_not_a_thunk() {
    // `saved is mine 1` is the whole claim: the guest compared what `signal`
    // returned against its own function. A runtime that handed back the arm64
    // trampoline would print 0 here and would install *that* on the next line.
    let Some(path) = signal_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (_, out, err) = run_guest(&path, &[]);

    assert!(out.contains("saved is mine 1"), "{out:?} (stderr: {err})");
    assert!(out.contains("queried is mine 1"), "{out:?} (stderr: {err})");
}

#[test]
fn sig_ign_is_installed_as_a_disposition_and_not_as_a_handler() {
    // Three raises, two handler lines: the middle one lands while `SIG_IGN` is
    // installed. A `SIG_IGN` wrapped in a thunk would make it three, and every
    // other assertion here would still pass.
    let Some(path) = signal_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (_, out, err) = run_guest(&path, &[]);

    assert_eq!(
        out.matches("handler saw 30").count(),
        2,
        "{out:?} (stderr: {err})"
    );
    assert!(out.contains("hits 3"), "{out:?} (stderr: {err})");
}

#[test]
fn the_two_architectures_lay_struct_sigaction_out_the_same_way() {
    // `sigaction`'s struct is passed to the host by copying sixteen bytes the
    // guest wrote. That is only sound because the layout is identical on both
    // architectures — so the same source is compiled for the *host* and run,
    // and its whole output must match the translated x86_64 build's. The first
    // line is the sizes; the rest is the behaviour they enable.
    let Some(guest) = signal_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let Some(native) = build_native("signals_native", SIGNAL_C) else {
        skip!("cannot build for the host")
    };
    let Some((_, native_out)) = run_native(&native) else {
        skip!("cannot run the host build")
    };
    let (_, guest_out, err) = run_guest(&guest, &[]);

    assert_eq!(guest_out, native_out, "stderr: {err}");
    assert_eq!(
        native_out, EXPECTED,
        "the fixture's own expectations have drifted"
    );
}

#[test]
fn the_untranslated_binary_agrees_line_for_line() {
    let Some(path) = signal_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let Some((native_status, native_out)) = run_native(&path) else {
        skip!("cannot run x86_64 binaries natively")
    };
    let (status, out, err) = run_guest(&path, &[]);
    assert_eq!(
        (status, out.as_str()),
        (native_status, native_out.as_str()),
        "stderr: {err}"
    );
}

#[test]
fn an_sa_siginfo_handler_is_refused_by_name() {
    // The boundary. A three-argument handler is called with the machine state at
    // the interrupted instruction, which here is inside a Cranelift-compiled
    // translation with no map back to the guest's registers — so the runtime
    // stops at *installation*, before anything could be delivered to it, rather
    // than synthesizing a plausible `mcontext64`. The work item the message
    // names belongs to the recompiler.
    let Some(path) = siginfo_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_ne!(
        status,
        Some(0),
        "SA_SIGINFO should not silently work: {out}"
    );
    assert!(
        err.contains("sigaction"),
        "the refusal should name the symbol: {err}"
    );
    assert!(
        err.contains("ucontext"),
        "the refusal should give the reason: {err}"
    );
    assert!(
        !out.contains("detailed"),
        "the handler must not have run: {out}"
    );
}
