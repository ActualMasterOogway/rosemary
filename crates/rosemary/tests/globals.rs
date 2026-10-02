mod common;

use common::{build_native_with, globals_fixture, run_guest_env, run_native};

const EXPECTED: &str = "mode 21\ncommon 21\nnull allocator 1\nmade 3\n";

#[test]
fn a_global_the_guest_loads_through_holds_the_object() {
    let Some(path) = globals_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest_env(&[("ROSEMARY_OBJC", "builtin")], &path, &[]);
    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, EXPECTED, "stderr: {err}");
}

/// And Apple's own frameworks say the same thing, which is the only
/// specification of these globals worth having.
#[test]
fn the_guest_agrees_with_the_same_program_built_for_the_host() {
    let Some(guest) = globals_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let Some(native) = build_native_with(
        "cfglobals_native",
        common::GLOBALS_C,
        &["-framework", "CoreFoundation"],
    ) else {
        skip!("cannot build native fixtures")
    };
    let (_, guest_out, err) = run_guest_env(&[("ROSEMARY_OBJC", "builtin")], &guest, &[]);
    let Some((_, native_out)) = run_native(&native) else {
        skip!("cannot run native fixtures")
    };
    assert_eq!(guest_out, native_out, "stderr: {err}");
}
