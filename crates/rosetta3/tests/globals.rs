//! A framework's data globals are **variables**, not the objects they hold.
//!
//! `kCFRunLoopDefaultMode` is a `CFStringRef` variable: the guest loads the
//! symbol's address out of its GOT and then loads the string out of that. A
//! provider that defines the symbol as the string itself hands the guest the
//! string's `isa`, and `kCFAllocatorDefault` - whose value is null - hands it
//! address zero.
//!
//! Neither failure names a symbol. The first is a length read out of a class
//! structure, which is a plausible number; the second is a segfault at zero
//! inside whatever the guest called next. Both are what this fixture would have
//! shown from the first run of the run loop, and what the in-crate tests could
//! not: they call the accessors directly and never do the guest's second load.
//!
//! Under `ROSETTA3_OBJC=builtin` deliberately. These are the *reimplemented*
//! frameworks' globals; under the native runtime the guest binds Apple's own,
//! which are correct by construction and would prove nothing about this tree.

mod common;

use common::{build_native_with, globals_fixture, run_guest_env, run_native};

const EXPECTED: &str = "mode 21\ncommon 21\nnull allocator 1\nmade 3\n";

#[test]
fn a_global_the_guest_loads_through_holds_the_object() {
    let Some(path) = globals_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest_env(&[("ROSETTA3_OBJC", "builtin")], &path, &[]);
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
    let (_, guest_out, err) = run_guest_env(&[("ROSETTA3_OBJC", "builtin")], &guest, &[]);
    let Some((_, native_out)) = run_native(&native) else {
        skip!("cannot run native fixtures")
    };
    assert_eq!(guest_out, native_out, "stderr: {err}");
}
