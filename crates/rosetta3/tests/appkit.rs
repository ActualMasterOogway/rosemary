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
