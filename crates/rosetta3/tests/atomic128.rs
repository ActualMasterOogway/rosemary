mod common;

use common::{build_native, cmpxchg16b_fixture, run_guest, run_native};

const EXPECTED: &str = "lo 40000 hi 40000 torn 0\n";

#[test]
fn a_contended_128_bit_swap_loses_no_updates() {
    let Some(path) = cmpxchg16b_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, EXPECTED, "stderr: {err}");
}

#[test]
fn the_guest_agrees_with_the_same_program_built_for_the_host() {
    let Some(guest) = cmpxchg16b_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let Some(native) = build_native("cmpxchg16b_native", common::CMPXCHG16B_C) else {
        skip!("cannot build native fixtures")
    };
    let (_, guest_out, err) = run_guest(&guest, &[]);
    let Some((_, native_out)) = run_native(&native) else {
        skip!("the native fixture would not run")
    };

    assert_eq!(guest_out, native_out, "stderr: {err}");
}
