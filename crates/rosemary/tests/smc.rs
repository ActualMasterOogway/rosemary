mod common;

use common::{patched_call_fixture, run_guest};

#[test]
fn a_patched_call_in_generated_code_reaches_its_new_target() {
    let Some(path) = patched_call_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, "before 3 beside 4 after 6 again 3\n", "stderr: {err}");
}
