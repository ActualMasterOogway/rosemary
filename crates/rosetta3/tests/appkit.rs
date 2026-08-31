//! `NSApplicationMain`: the nib, the delegate, and the launch notifications.
//!
//! The first thing a Cocoa program's `main` calls, and until it answers nothing
//! a guest wrote ever runs. What each line of the fixture's output would
//! falsify:
//!
//! - **`will` and `did`, in that order, each naming itself** - the two launch
//!   notifications arrived and the argument is a real `NSNotification` rather
//!   than whatever was in the register;
//! - **`delegate is self 1`** - the delegate was wired *from the nib*, which
//!   means the keyed archive was decoded, the `NSCustomObject` was instantiated
//!   by the class name it recorded, and the outlet reached `-setDelegate:`
//!   through key-value coding. The fixture never sets it in code;
//! - **the property lines** - a `@property`'s name and attribute string survived
//!   the replay of the guest's `__objc_data` into the runtime. Nothing else in
//!   this suite asks: to a program introspecting a class, a runtime that drops
//!   the metadata answers exactly as a class with no properties does.
//!
//! Under `ROSETTA3_OBJC=builtin` deliberately. This tree's AppKit is published
//! only under the builtin runtime, for the reason `frameworks.rs` gives - so
//! under the native one the guest binds Apple's, which would test Apple's.

mod common;

use common::{appkit_fixture, run_guest_env};

fn launch() -> Option<(Option<i32>, String, String)> {
    let path = appkit_fixture()?;
    Some(run_guest_env(&[("ROSETTA3_OBJC", "builtin")], &path, &[]))
}

#[test]
fn the_delegate_wired_in_the_nib_is_told_the_application_launched() {
    let Some((status, out, err)) = launch() else {
        skip!("cannot build x86_64 fixtures")
    };
    assert_eq!(status, Some(0), "stdout: {out}\nstderr: {err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines.first(),
        Some(&"will NSApplicationWillFinishLaunchingNotification"),
        "stdout: {out}\nstderr: {err}"
    );
    assert_eq!(
        lines.get(1),
        Some(&"did NSApplicationDidFinishLaunchingNotification"),
        "stdout: {out}\nstderr: {err}"
    );
    assert!(
        out.contains("delegate is self 1\n"),
        "the nib's delegate outlet did not reach the application: {out}\nstderr: {err}"
    );
}

/// The `@property` the guest declared is one the runtime can be asked about.
///
/// Both halves: the list has it, and its attribute string takes apart into the
/// fields clang wrote - `Ti` for an `int`, `N` for `nonatomic`, `V_launches`
/// for the backing ivar.
#[test]
fn a_guests_property_metadata_survives_the_replay() {
    let Some((status, out, err)) = launch() else {
        skip!("cannot build x86_64 fixtures")
    };
    assert_eq!(status, Some(0), "stdout: {out}\nstderr: {err}");
    assert!(
        out.contains("properties 1\n"),
        "the class reported no properties: {out}\nstderr: {err}"
    );
    assert!(
        out.contains("property launches Ti,N,V_launches\n"),
        "the attribute string is not the one clang emitted: {out}\nstderr: {err}"
    );
    assert!(
        out.contains("attributes 3 first T=i\n"),
        "the attribute string did not take apart: {out}\nstderr: {err}"
    );
}
