//! 128-bit division, which is a call on every machine this runs on.
//!
//! `unsigned __int128 / unsigned __int128` has no instruction on x86-64 or on
//! arm64, so a compiler emits a call to compiler-rt's `__udivti3`. Below Zero
//! reached one after its start screen loaded, which is the ordinary way these
//! turn up: nothing imports them until some generated or templated code divides
//! a 128-bit value.
//!
//! What each test would falsify:
//!
//! - **both halves of each argument arrive** — an `__int128` spans a register
//!   pair, and the dividend here does not fit in sixty-four bits, so a shim
//!   reading only the low halves gets a different number rather than a
//!   suspicious one;
//! - **the signed forms truncate toward zero**, with the remainder taking the
//!   numerator's sign, which is where a `floor`-style implementation would
//!   differ;
//! - **the guest agrees with the host**, which is the only statement that
//!   covers the register pairing on both sides at once.

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
