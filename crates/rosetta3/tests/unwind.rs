//! Milestone 5: guest C++ exceptions, unwound by the runtime's own unwinder.
//!
//! Through the built driver, because the claim is about a process: a `throw`
//! three frames down, destructors in order, a handler chosen by type, and
//! execution carrying on afterwards.
//!
//! The assertion that matters most is equality with the *untranslated* binary.
//! There is no other specification of what a C++ exception does that is worth
//! writing into a test — every clause of the fixture is a sentence from the
//! standard, and the machine already knows how to say it.
//!
//! The refusals are tested just as hard as the successes. Each names a case the
//! unwinder cannot do rather than doing it wrongly, and each is the test that
//! must *change* when that case is built.

mod common;

use common::{exceptions_fixture, run_guest, run_native, unwind_refusals_fixture};

/// What the fixture prints when unwinding works, and what the same binary prints
/// natively. Every line is a separate claim; see [`common::EXCEPTIONS_CPP`].
const EXPECTED: &str = "\
~level3
~level2
~level1
caught Boom 6
~Boom(6)
outer caught inner
caught something
same frame 11
by value by value
base caught 4
std my error
rethrowing 1
finally 1
~Boom(1)
~thread
thread caught 7
~Boom(7)
thread returned 1
done
";

#[test]
fn a_guest_throw_is_caught_and_the_program_carries_on() {
    let Some(path) = exceptions_fixture() else {
        skip!("cannot build x86_64 C++ fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, EXPECTED, "stderr: {err}");
}

#[test]
fn the_translated_program_agrees_with_the_untranslated_one() {
    let Some(path) = exceptions_fixture() else {
        skip!("cannot build x86_64 C++ fixtures")
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
    assert_eq!(
        native_out, EXPECTED,
        "the fixture's own expectations have drifted"
    );
}

#[test]
fn destructors_run_in_order_before_the_handler() {
    // The whole of phase two in one assertion. A runtime that entered the
    // handler directly would print the cleanups late, in the wrong order, or —
    // most likely — not at all, and the array's own order is the claim.
    let Some(path) = exceptions_fixture() else {
        skip!("cannot build x86_64 C++ fixtures")
    };
    let (_, out, err) = run_guest(&path, &[]);
    let lines: Vec<&str> = out.lines().collect();

    assert_eq!(
        &lines[..5],
        &["~level3", "~level2", "~level1", "caught Boom 6", "~Boom(6)"],
        "stderr: {err}"
    );
}

#[test]
fn a_handler_is_chosen_by_type_and_not_by_position() {
    // The nested `try` throws `Other` past a `catch (const Boom &)`. A runtime
    // that entered the first landing pad it found would print the BUG line the
    // fixture plants there, and everything after it would still look right.
    let Some(path) = exceptions_fixture() else {
        skip!("cannot build x86_64 C++ fixtures")
    };
    let (_, out, err) = run_guest(&path, &[]);

    assert!(
        !out.contains("BUG:"),
        "the wrong clause ran:\n{out}\n(stderr: {err})"
    );
    assert!(out.contains("outer caught inner"), "{out:?}");
}

#[test]
fn a_base_class_clause_catches_a_derived_type() {
    // Both directions of the single-inheritance walk: a base defined in the
    // guest, and `std::exception`, whose `type_info` is the host's and reached
    // through the guest's own import of it.
    let Some(path) = exceptions_fixture() else {
        skip!("cannot build x86_64 C++ fixtures")
    };
    let (_, out, err) = run_guest(&path, &[]);

    assert!(out.contains("base caught 4"), "{out:?} (stderr: {err})");
    assert!(out.contains("std my error"), "{out:?} (stderr: {err})");
}

#[test]
fn a_rethrown_exception_survives_the_handler_that_caught_it() {
    // `throw;` continues the same exception outward. The failure this catches is
    // freeing it at the end of the handler that rethrew: the outer `catch` would
    // then bind to memory that had been returned, and would usually still print
    // the right number.
    let Some(path) = exceptions_fixture() else {
        skip!("cannot build x86_64 C++ fixtures")
    };
    let (_, out, err) = run_guest(&path, &[]);
    let lines: Vec<&str> = out.lines().collect();
    let at = lines
        .iter()
        .position(|l| *l == "rethrowing 1")
        .expect("the rethrow ran");

    assert_eq!(
        &lines[at..at + 3],
        &["rethrowing 1", "finally 1", "~Boom(1)"],
        "stderr: {err}"
    );
}

#[test]
fn a_thread_unwinds_its_own_stack() {
    // Per-thread in-flight state. Two walks sharing one would be a race in a
    // fixture with one thread throwing, and a corruption in any real program.
    let Some(path) = exceptions_fixture() else {
        skip!("cannot build x86_64 C++ fixtures")
    };
    let (_, out, err) = run_guest(&path, &[]);

    assert!(out.contains("thread caught 7"), "{out:?} (stderr: {err})");
    assert!(
        out.contains("thread returned 1"),
        "the start routine's value: {out:?}"
    );
}

#[test]
fn an_uncaught_exception_stops_without_running_destructors() {
    // C++ requires `std::terminate` with the stack *intact*, which is the whole
    // reason the search is a separate phase. `~uncaught` printing here would
    // mean the runtime unwound first and asked afterwards.
    let Some(path) = unwind_refusals_fixture() else {
        skip!("cannot build x86_64 C++ fixtures")
    };
    let (status, out, err) = run_guest(&path, &["uncaught"]);

    assert_ne!(
        status,
        Some(0),
        "an uncaught exception should not succeed: {out}"
    );
    assert!(
        err.contains("terminate"),
        "the stop should name the case: {err}"
    );
    assert!(
        !out.contains("~uncaught"),
        "a destructor ran before terminate:\n{out}"
    );
    assert!(
        !out.contains("after"),
        "execution continued past the throw:\n{out}"
    );
}

#[test]
fn a_throw_across_a_host_frame_is_refused_by_name() {
    // The comparator is called by the host's `qsort`, so the handler in `main`
    // is on the far side of host frames. Unwinding them is the same problem
    // `longjmp` refuses, and the message says which case it is rather than
    // reporting the generic "nothing catches this".
    let Some(path) = unwind_refusals_fixture() else {
        skip!("cannot build x86_64 C++ fixtures")
    };
    let (status, out, err) = run_guest(&path, &["across"]);

    assert_ne!(status, Some(0), "{out}");
    assert!(
        err.contains("host frames"),
        "the refusal should name the case: {err}"
    );
    assert!(
        !out.contains("caught across"),
        "it cannot actually have been caught:\n{out}"
    );
}

#[test]
fn a_catch_needing_multiple_inheritance_is_refused_rather_than_missed() {
    // A base subobject at a non-zero offset would have to move the pointer the
    // `catch` binds to. Reporting "nothing catches this" would be a lie of the
    // most expensive kind — the program would terminate and the clause that
    // should have run is right there in the source.
    let Some(path) = unwind_refusals_fixture() else {
        skip!("cannot build x86_64 C++ fixtures")
    };
    let (status, _, err) = run_guest(&path, &["multiple"]);

    assert_ne!(status, Some(0));
    assert!(
        err.contains("type_info"),
        "the refusal should name what is missing: {err}"
    );
    assert!(
        !err.contains("std::terminate"),
        "this is not 'nobody catches it': {err}"
    );
}

#[test]
fn the_slow_import_policy_unwinds_identically() {
    // `--slow` sends every promotable import through the marshaller instead of
    // inlining it. The unwinder reads the guest's registers at the moment of the
    // throw, so a policy that changed where those registers came from would show
    // up here and nowhere else.
    let Some(path) = exceptions_fixture() else {
        skip!("cannot build x86_64 C++ fixtures")
    };
    let (fast_status, fast_out, err) = run_guest(&path, &[]);
    let (slow_status, slow_out, slow_err) = common::run_guest_with(&["--slow"], &path, &[]);

    assert_eq!(fast_status, Some(0), "stderr: {err}");
    assert_eq!(slow_status, Some(0), "stderr: {slow_err}");
    assert_eq!(fast_out, slow_out);
}
