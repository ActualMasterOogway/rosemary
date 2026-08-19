//! The native backend: a host-architecture program, loaded by machish and run.
//!
//! Every claim here is differential — the same binary run twice, once by the
//! kernel and once by this runtime — because that is the only comparison that
//! cannot be satisfied by a loader which merely does not crash.
//!
//! - **hello** — the link produced something callable, and `argv` reached it.
//! - **constructors** — `__mod_init_func` ran, in order, before `main`.
//! - **C++** — the host's own libc++ was reached, not the vendored guest copy.
//! - **selection** — a binary goes to the backend that can actually run it,
//!   `/bin/ls`'s arm64e slice included.

mod common;

use common::{build_native, build_native_with, driver, run_guest_with, run_native};

/// Run `path` through the driver, letting it choose a backend.
fn run(path: &str, args: &[&str]) -> (Option<i32>, String, String) {
    run_guest_with(&[], path, args)
}

#[test]
fn a_native_hello_world_matches_the_kernels_own_run_of_it() {
    let Some(path) = build_native("native_hello", common::HELLO_C) else {
        skip!("cannot build native fixtures")
    };
    let Some((native_status, native_out)) = run_native(&path) else {
        skip!("cannot run the native fixture")
    };
    let (status, out, err) = run(&path, &[]);

    assert_eq!(status, native_status, "stderr: {err}");
    assert_eq!(out, native_out, "stderr: {err}");
}

#[test]
fn a_native_exit_status_reaches_the_shell() {
    let Some(path) = build_native("native_hello", common::HELLO_C) else {
        skip!("cannot build native fixtures")
    };
    // `main` returns `abs(argc) - 1`, so one argument makes it 1 — and a
    // runtime that dropped the status would report 0 for both.
    let (status, _, err) = run(&path, &["one"]);
    assert_eq!(status, Some(1), "stderr: {err}");
}

#[test]
fn native_constructors_run_in_order_before_main() {
    let Some(path) = build_native("native_ctors", common::CTOR_C) else {
        skip!("cannot build native fixtures")
    };
    let Some((_, native_out)) = run_native(&path) else {
        skip!("cannot run the native fixture")
    };
    let (status, out, err) = run(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, native_out, "stderr: {err}");
}

/// The vendored libc++ is x86-64 and is not in the native provider chain, so a
/// C++ fixture that prints the right thing proves the *host's* copy was linked.
#[test]
fn a_native_cxx_program_reaches_the_hosts_own_libcxx() {
    let Some(path) = build_native_with("native_cxx", CXX, &["-x", "c++", "-lc++"]) else {
        skip!("cannot build native C++ fixtures")
    };
    let Some((_, native_out)) = run_native(&path) else {
        skip!("cannot run the native fixture")
    };
    let (status, out, err) = run(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, native_out, "stderr: {err}");
}

const CXX: &str = r#"
#include <iostream>
#include <string>
#include <vector>

int main() {
    std::vector<std::string> v{"a", "bb", "ccc"};
    for (const auto &s : v) std::cout << s << " " << s.size() << "\n";
    return 0;
}
"#;

#[test]
fn an_x86_64_binary_still_takes_the_emulated_path() {
    let Some(path) = common::hello() else {
        skip!("cannot build x86_64 fixtures")
    };
    // The emulated backend is the only one that translates, so a non-zero
    // region count is what distinguishes the two without a flag to ask with.
    let (status, _, err) = common::run_guest_env(&[("ROSETTA3_TRACE_VM", "1")], &path, &[]);
    assert_eq!(status, Some(0), "stderr: {err}");
    assert!(
        err.contains("image "),
        "the emulated backend reports an image map: {err}"
    );
}

#[test]
fn forcing_native_on_an_x86_64_binary_names_the_architecture() {
    let Some(path) = common::hello() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, _, err) = run_guest_with(&["--native"], &path, &[]);

    assert_eq!(status, Some(1));
    assert!(err.contains("native backend"), "{err}");
    assert!(err.contains("x86-64"), "{err}");
}

#[test]
fn forcing_emulated_on_a_native_binary_names_the_architecture() {
    let Some(path) = build_native("native_hello", common::HELLO_C) else {
        skip!("cannot build native fixtures")
    };
    let (status, _, err) = run_guest_with(&["--emulated"], &path, &[]);

    assert_eq!(status, Some(1));
    assert!(err.contains("emulated backend"), "{err}");
}

/// `/bin/ls` is x86-64 plus arm64e, and machish refuses arm64e's authenticated
/// chained pointers. Selection has to know that before the loader finds out, or
/// a binary that used to run under emulation stops running at all.
#[test]
fn an_arm64e_slice_does_not_claim_the_native_backend() {
    if !std::path::Path::new("/bin/ls").exists() {
        skip!("no /bin/ls")
    }
    let (status, _, err) = run_guest_with(&["--native"], "/bin/ls", &[]);
    assert_eq!(status, Some(1));
    assert!(err.contains("arm64e"), "{err}");

    // And with nothing forced it runs, which is the part that would regress.
    let out = std::process::Command::new(driver())
        .args(["run", "/bin/ls", "/bin"])
        .output()
        .expect("running the driver");
    assert_eq!(out.status.code(), Some(0));
}
