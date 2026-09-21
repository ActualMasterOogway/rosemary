mod common;

use common::{longjmp_across_fixture, run_guest, run_guest_with, run_native, setjmp_fixture};

const EXPECTED: &str = "\
first return
level1
level2
level3
came back with 42 (trip 1)
level1
level2
level3
came back with 1 (trip 2)
plain spelling ok
masked spelling ok
guard gg len 255 trips 2 total 43
";

#[test]
fn a_longjmp_unwinds_three_guest_frames_and_execution_continues() {
    let Some(path) = setjmp_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    // The `BUG:` lines are planted after every call that must not return, so a
    // no-op `longjmp` fails here with the reason in the diff rather than as a
    // count.
    assert!(!out.contains("BUG:"), "a longjmp fell through:\n{out}");
    assert_eq!(out, EXPECTED, "stderr: {err}");
}

#[test]
fn the_translated_program_agrees_with_the_untranslated_one() {
    let Some(path) = setjmp_fixture() else {
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
    assert_eq!(
        native_out, EXPECTED,
        "the fixture's own expectations have drifted"
    );
}

#[test]
fn a_longjmp_of_zero_arrives_as_one() {
    // The second trip jumps with 0 and `setjmp` must report 1 - the one place
    // `longjmp` is specified to change the value it was given, and the kind of
    // detail a hand-written implementation drops.
    let Some(path) = setjmp_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (_, out, err) = run_guest(&path, &[]);
    assert!(
        out.contains("came back with 1 (trip 2)"),
        "{out:?} (stderr: {err})"
    );
}

#[test]
fn both_darwin_spellings_reach_the_same_implementation() {
    // `_setjmp`/`_longjmp` are imported as `__setjmp`/`__longjmp` and
    // `sigsetjmp`/`siglongjmp` under their own names: three import pairs, one
    // shim. A table that knew only `_setjmp` would refuse two of them by name.
    let Some(path) = setjmp_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (_, out, err) = run_guest(&path, &[]);
    assert!(out.contains("plain spelling ok"), "{out:?} (stderr: {err})");
    assert!(
        out.contains("masked spelling ok"),
        "{out:?} (stderr: {err})"
    );
}

#[test]
fn setjmp_is_never_promoted_to_an_inlined_import() {
    // `int setjmp(jmp_buf)` is an ordinary marshallable signature, so nothing
    // about its *types* keeps the lifter from inlining a call to the host's -
    // which would save arm64 callee-saved registers and the JIT's own frame,
    // and would not fault. `ImportPolicy` refuses to promote a shimmed symbol,
    // and this is that claim from the outside: the default (fast where possible)
    // and `--slow` produce the same output.
    let Some(path) = setjmp_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (fast_status, fast_out, err) = run_guest(&path, &[]);
    let (slow_status, slow_out, slow_err) = run_guest_with(&["--slow", "--emulated"], &path, &[]);

    assert_eq!(fast_status, Some(0), "stderr: {err}");
    assert_eq!(slow_status, Some(0), "stderr: {slow_err}");
    assert_eq!(fast_out, slow_out);
    assert_eq!(fast_out, EXPECTED);
}

#[test]
fn a_longjmp_across_a_host_frame_is_refused_by_name() {
    // The comparator is called by the host's `qsort`, so its `setjmp` lives in
    // an outer VM entry with host frames in between. Restoring the guest context
    // there would leave `qsort`'s arm64 frame on the host stack with nothing to
    // return through it - so the runtime stops and says which case it is. This
    // is the boundary of what is built, stated as a test rather than a comment,
    // and it is the test that must *change* when the guest unwinder lands.
    //
    // It is also, since imports became bridged calls, the test that holds
    // [`rosetta3::bridge`] honest about parked failures. The comparator's
    // refusal cannot be returned through the host's `qsort` frame, so it is left
    // on the thread - and `qsort` itself is now an inline bridged call that
    // produces no exit for the run loop to notice it at. A bridge that reported
    // success without looking would let this program sort nothing and exit zero.
    let Some(path) = longjmp_across_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_ne!(
        status,
        Some(0),
        "a longjmp across host frames should not succeed: {out}"
    );
    assert!(
        err.contains("longjmp"),
        "the refusal should name the symbol: {err}"
    );
    assert!(
        err.contains("unwind"),
        "the refusal should give the reason: {err}"
    );
    // And it is a diagnostic, not a fault: the process exited on its own terms.
    assert!(
        status.is_some(),
        "the guest crashed instead of being stopped: {err}"
    );
}
