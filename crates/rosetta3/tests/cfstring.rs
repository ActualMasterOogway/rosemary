//! A `CFString` converted to bytes, which is how a Cocoa guest reads any name.
//!
//! The entry for `CFStringGetCString` declared three parameters where the
//! function has four, so the host read whatever the guest left in `x3` as the
//! encoding and answered `false`. Nothing failed at the call: the conversion
//! produced an empty string, and Unity appended that empty string to
//! `Contents/MacOS/` to build a plugin path - so every `.bundle` plugin in
//! Subnautica Below Zero was `dlopen`ed as a directory and came back as
//! `DllNotFoundException`.
//!
//! What each test would falsify:
//!
//! - **the conversion succeeds and produces the right bytes**, which is the
//!   whole of the bug;
//! - **both architectures agree** - the same source built for arm64 prints the
//!   same line, so the length and the buffer size are checked too and not just
//!   the string.

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
