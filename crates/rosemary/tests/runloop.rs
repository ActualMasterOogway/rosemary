mod common;

fn run_builtin(path: &str) -> (Option<i32>, String, String) {
    common::run_guest_env(&[("ROSEMARY_OBJC", "builtin")], path, &[])
}

#[test]
fn a_guest_is_called_back_from_inside_the_run_loop() {
    let Some(path) = common::runloop_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_builtin(&path);
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
    let (_, guest_out, err) = run_builtin(&guest);
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

#[test]
fn a_framework_this_tree_implements_can_be_opened_by_name() {
    let Some(guest) = common::dlopen_framework_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let Some(native) =
        common::build_native_with("dlopen_framework_native", common::DLOPEN_FRAMEWORK_C, &[])
    else {
        skip!("cannot build native fixtures")
    };
    let (status, guest_out, err) = common::run_guest(&guest, &[]);
    assert_eq!(status, Some(0), "stderr: {err}");
    let Some((_, native_out)) = common::run_native(&native) else {
        skip!("the native fixture would not run")
    };
    assert_eq!(guest_out, native_out, "stderr: {err}");
}
