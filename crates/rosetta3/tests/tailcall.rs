mod common;

use common::{run_guest, run_guest_with, tailcall_fixture};

const EXPECTED: &str = "rc 0 host 127.0.0.1 serv 80\n";

#[test]
fn a_spilled_argument_survives_a_tail_jump_to_an_import() {
    let Some(path) = tailcall_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, EXPECTED, "stderr: {err}");
}

#[test]
fn the_marshaller_places_it_as_well_as_the_lifter_does() {
    // The same program with import promotion off, so every argument goes
    // through `abi::marshal` rather than through code microlift emitted. This
    // is the arrangement that was wrong, and the reason the fast path hid it:
    // both must agree, and agreeing with the native run is what says which.
    let Some(path) = tailcall_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest_with(&["--slow", "--emulated"], &path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, EXPECTED, "stderr: {err}");
}
