mod common;

use common::{build_native, int128_fixture, run_guest, run_native};

#[test]
fn a_guest_divides_128_bit_integers() {
    let Some(path) = int128_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    // 2^100 + 12345 over 3, as two 64-bit halves.
    assert!(
        out.contains("uq 22906492245:6148914691236521320"),
        "{out:?}"
    );
    assert!(out.contains("ur 0:1"), "{out:?}");
}

#[test]
fn the_guest_agrees_with_the_same_program_built_for_the_host() {
    let Some(guest) = int128_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let Some(native) = build_native("int128_native", common::INT128_C) else {
        skip!("cannot build native fixtures")
    };
    let (_, guest_out, err) = run_guest(&guest, &[]);
    let Some((_, native_out)) = run_native(&native) else {
        skip!("the native fixture would not run")
    };

    assert_eq!(guest_out, native_out, "stderr: {err}");
}
