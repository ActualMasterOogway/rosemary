//! A tail `jmp` to an import puts its stack arguments one slot higher.
//!
//! Microlift exits an intercepted `call` *before* the return address is pushed,
//! so a spilled argument is at `rsp + offset`. A compiler that tail-jumps to an
//! import leaves the original return address on the stack and reuses the
//! incoming argument area above it, so the same argument is at `rsp + 8 +
//! offset`. The marshaller read the first offset for both.
//!
//! What each test would falsify:
//!
//! - **the flags word survives the tail call** — it is the seventh integer
//!   argument, so SysV spills it, and reading the return address in its place
//!   turns `NI_NUMERICHOST | NI_NUMERICSERV` into something else entirely;
//! - **through the marshaller specifically** — `--slow` refuses the promotion
//!   that would let microlift place the arguments itself, which is what made
//!   this invisible: on the fast path the lifter already got it right;
//! - **and the ordinary `call` still works** — the fix is an adjustment for one
//!   case, so the other case is the one it could break.

mod common;

use common::{run_guest, run_guest_with, tailcall_fixture};

const EXPECTED: &str = "rc 0 host 127.0.0.1 serv 80\n";

#[test]
fn a_spilled_argument_survives_a_tail_jump_to_an_import() {
    let Some(path) = tailcall_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, EXPECTED, "stderr: {err}");
}

#[test]
fn the_marshaller_places_it_as_well_as_the_lifter_does() {
    // The same program with import promotion off, so every argument goes
    // through `abi::marshal` rather than through code microlift emitted. This
    // is the arrangement that was wrong, and the reason the fast path hid it:
    // both must agree, and agreeing with the native run is what says which.
    let Some(path) = tailcall_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest_with(&["--slow"], &path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, EXPECTED, "stderr: {err}");
}
