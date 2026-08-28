//! The run loop, with guest code on the other side of both callbacks.
//!
//! What each test would falsify:
//!
//! - **a guest's `perform` runs from inside `CFRunLoopRunInMode`**, which means
//!   the function pointer buried in the `CFRunLoopSourceContext` the guest built
//!   on its stack became a trampoline on the way in - the ordinary callback path
//!   cannot see it, because it looks at registers;
//! - **a timer fires repeatedly and its `info` arrives**, which is the same
//!   claim for a callback that *is* in a register and a context that is not;
//! - **the same program prints the same thing natively**, which is the only
//!   specification of a run loop worth having.

mod common;

#[test]
fn a_guest_is_called_back_from_inside_the_run_loop() {
    let Some(path) = common::runloop_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = common::run_guest(&path, &[]);
    assert_eq!(status, Some(0), "stderr: {err}");
    assert!(
        out.contains("source 1 info 42\n"),
        "the source's callback did not reach the guest: {out}\nstderr: {err}"
    );
    assert!(
        out.contains("timer 1 info 7\n"),
        "the timer's callback did not reach the guest: {out}\nstderr: {err}"
    );
    assert!(
        out.contains("done 1 1\n"),
        "both had to fire at least twice: {out}\nstderr: {err}"
    );
    assert!(
        !out.contains("the loop emptied"),
        "the loop went empty with a live source and timer in it: {out}"
    );
}

/// And it agrees with the same program built for the host.
///
/// The orderings a run loop produces are not fully determined - a timer and a
/// source that both come due are serviced in one turn - so this compares the
/// *set* of lines rather than the sequence, which is the strongest claim that is
/// actually true of both.
#[test]
fn the_guest_agrees_with_the_same_program_built_for_the_host() {
    let Some(guest) = common::runloop_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let Some(native) = common::build_native_with(
        "runloop_native",
        common::RUNLOOP_C,
        &["-framework", "CoreFoundation"],
    ) else {
        skip!("cannot build native fixtures")
    };
    let (_, guest_out, err) = common::run_guest(&guest, &[]);
    let Some((_, native_out)) = common::run_native(&native) else {
        skip!("the native fixture would not run")
    };

    let sorted = |text: &str| {
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        lines.sort();
        lines
    };
    assert_eq!(sorted(&guest_out), sorted(&native_out), "stderr: {err}");
}
