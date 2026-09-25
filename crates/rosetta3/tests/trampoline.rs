mod common;

use common::{run_guest, run_guest_env};

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
