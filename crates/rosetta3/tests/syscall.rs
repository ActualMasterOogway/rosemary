mod common;

use common::{run_guest, syscall_fixture};

#[test]
fn a_guest_syscall_stops_the_run_and_says_which_one() {
    let Some(path) = syscall_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    // It got as far as the `syscall` and no further: the guest was not resumed
    // past an event nothing serviced.
    assert!(out.contains("before"), "{out:?} (stderr: {err})");
    assert!(!out.contains("raw"), "the syscall was performed: {out:?}");
    assert!(!out.contains("after"), "{out:?} (stderr: {err})");
    // A refusal, not a crash: the diverted PC has to reach the run loop rather
    // than being jumped to.
    assert_eq!(status, Some(1), "stderr: {err}");
    assert!(!err.contains("SIGSEGV"), "stderr: {err}");

    // `write` is `0x2000004` on Darwin, and the number is the point: a refusal
    // that did not name the call would leave the reader to find it themselves.
    assert!(err.contains("guest syscall 0x2000004"), "stderr: {err}");
}
