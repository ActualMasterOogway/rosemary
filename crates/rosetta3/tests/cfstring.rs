mod common;

use common::{build_native_with, cfstring_fixture, run_guest, run_native};

const EXPECTED: &str = "len 10 max 30 ok 1 buf 'fmodstudio'\n";

#[test]
fn a_cfstring_converts_to_the_bytes_it_holds() {
    let Some(path) = cfstring_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, EXPECTED, "stderr: {err}");
}

#[test]
fn the_guest_agrees_with_the_same_program_built_for_the_host() {
    let Some(guest) = cfstring_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let Some(native) = build_native_with(
        "cfstring_native",
        common::CFSTRING_C,
        &["-framework", "CoreFoundation"],
    ) else {
        skip!("cannot build native fixtures")
    };
    let (_, guest_out, err) = run_guest(&guest, &[]);
    let Some((_, native_out)) = run_native(&native) else {
        skip!("the native fixture would not run")
    };

    assert_eq!(guest_out, native_out, "stderr: {err}");
}
