//! A guest that calls the host through pointers, both ways round.
//!
//! `rosetta3::trampoline` substitutes the value of an import slot and the answer
//! `dlsym` gives, so that a call through a function *pointer* becomes the same
//! inline call a call *site* already got. The property that makes that an
//! optimisation rather than a change is the one these tests check: the program
//! prints the same thing with the substitution and without it.
//!
//! `ROSETTA3_TRAMPOLINES=0` is the without. It is a real baseline rather than a
//! debug switch — every path it turns off is a path that worked before this
//! existed — which is what lets the two runs be compared line for line.

mod common;

use common::{run_guest, run_guest_env};

/// What the fixture must print, whichever way the pointers were reached.
///
/// `same=1` is the load-bearing one: two routes to `strlen` — the import slot
/// and `dlsym` — must produce one pointer. A substitution that made a stub per
/// route would be observably wrong here, and nowhere else for a long time.
fn expect(out: &str, err: &str) {
    assert!(out.contains("slot len=6\n"), "{out:?} (stderr: {err})");
    assert!(
        out.contains("dlsym len=8 same=1\n"),
        "two routes to one symbol gave two pointers: {out:?} (stderr: {err})"
    );
    // The seventh argument is on the stack, and it is what decides whether the
    // address is rendered numerically or looked up. A stub that shifted the
    // stack arguments by a slot loses the flags word and gets a reverse DNS
    // answer, which is exactly what `tests/tailcall.rs` catches from the other
    // side.
    assert!(
        out.contains("gni rc=0 host=127.0.0.1 serv=80\n"),
        "a stack argument did not survive the call: {out:?} (stderr: {err})"
    );
}

#[test]
fn a_host_call_through_a_pointer_gets_the_right_answer() {
    let Some(path) = common::trampoline_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);
    assert_eq!(status, Some(0), "stderr: {err}");
    expect(&out, &err);
}

/// The same program with the substitution off, which is what makes the claim
/// "an optimisation, and nothing else" falsifiable.
#[test]
fn the_answer_is_the_same_without_trampolines() {
    let Some(path) = common::trampoline_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest_env(&[("ROSETTA3_TRAMPOLINES", "0")], &path, &[]);
    assert_eq!(status, Some(0), "stderr: {err}");
    expect(&out, &err);
}

/// And that the pointers really were substituted, so the test above is
/// measuring something.
///
/// Without this the pair would pass just as happily if the whole module had
/// never run: three calls through pointers, all serviced the old way, all
/// correct. `reached` is the number that says the guest actually entered a stub.
#[test]
fn the_stubs_are_the_path_the_guest_took() {
    let Some(path) = common::trampoline_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (_, _, err) = run_guest_env(&[("ROSETTA3_STATS", "1")], &path, &[]);
    let line = err
        .lines()
        .find(|l| l.contains("trampoline(s)"))
        .unwrap_or_else(|| panic!("no trampoline stats in: {err}"));
    let reached: u64 = line
        .split_whitespace()
        .zip(line.split_whitespace().skip(1))
        .find(|(_, word)| *word == "reached")
        .and_then(|(n, _)| n.parse().ok())
        .unwrap_or_else(|| panic!("unreadable stats line: {line:?}"));
    // `strlen` and `getnameinfo`: two symbols, however many times each is
    // called.
    assert!(reached >= 2, "the guest entered no stubs: {line:?}");
}
