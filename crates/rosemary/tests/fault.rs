mod common;

use common::{fault_fixture, run_guest};

#[test]
fn a_faulting_guest_names_its_address_and_its_region() {
    let Some(path) = fault_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    // It really faulted: the line before the store is there and the line after
    // it is not. Without this the rest could pass on a program that exited.
    assert!(out.contains("about to fault"), "{out:?} (stderr: {err})");
    assert!(!out.contains("still here"), "{out:?} (stderr: {err})");
    // And the process died of the signal rather than being handled past it.
    assert_ne!(status, Some(0), "stderr: {err}");

    assert!(err.contains("rosemary: fault: SIGSEGV"), "stderr: {err}");
    assert!(err.contains("0x4141414141414141"), "stderr: {err}");
    // The printable-address branch. `0x41` is `A`.
    assert!(err.contains("ascii \"AAAAAAAA\""), "stderr: {err}");
    // The point of the whole module: a guest range, not just a host pc.
    assert!(err.contains("guest region 0x"), "stderr: {err}");
    assert!(err.contains("host pc 0x"), "stderr: {err}");
    // Named against the image it came from, so the range is `objdump` input
    // rather than a number needing a second run to interpret.
    assert!(err.contains("(guest_fault+0x"), "stderr: {err}");
    // And the instruction itself, out of microlift's source locations. This is
    // the difference between a work item and a search: the faulting store is
    // one address, not a region to disassemble.
    assert!(err.contains("guest instruction 0x"), "stderr: {err}");
}

#[test]
fn the_report_names_the_thread_that_faulted() {
    let Some(path) = fault_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (_, _, err) = run_guest(&path, &[]);

    // A fault on the main thread is still a fault on *a* thread, and the guest
    // stack it names is what a later multi-threaded crash will be read against.
    assert!(err.contains("thread 0x"), "stderr: {err}");
    assert!(err.contains("guest rsp 0x"), "stderr: {err}");
    assert!(
        !err.contains("not a guest thread"),
        "the main thread is a guest thread here; stderr: {err}"
    );
}
