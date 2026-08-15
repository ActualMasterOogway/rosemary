//! Block linking: a translated region entering its successor without the run
//! loop.
//!
//! The mechanism is described in `microlift::core::link` and the host half in
//! [`rosetta3::cache`]; what is checked here is the property that makes it
//! usable rather than the machinery, and there are only two claims worth
//! testing at this level.
//!
//! **It changes nothing the guest can see.** Linking is a decision about *where
//! the answer to "what runs next" is computed*, so the same program run both
//! ways must produce the same output, byte for byte, including the paths where
//! it would be easiest to get wrong: a host callback into guest code, a
//! `longjmp` out of one, an indirect call through a function pointer.
//! `ROSETTA3_NO_BLOCK_LINK=1` exists so that this can be asserted rather than
//! argued.
//!
//! **And it happens.** A test that only checked the first claim would pass just
//! as well if no region ever linked, which is the failure mode a refactor
//! introduces silently. The counters say how many entries never reached the run
//! loop, so the second claim is one number.

mod common;

use common::{
    link_fixture, qsort_fixture, run_guest, run_guest_env, setjmp_fixture, thread_fixture,
};

/// The environment that turns the mechanism off. See `rosetta3::jit`.
const OFF: (&str, &str) = ("ROSETTA3_NO_BLOCK_LINK", "1");

/// Run `path` with linking on and with it off, and return both outputs.
fn both_ways(path: &str) -> (String, String) {
    let (linked_status, linked, err) = run_guest(path, &[]);
    assert_eq!(linked_status, Some(0), "linked run failed; stderr: {err}");
    let (plain_status, plain, err) = run_guest_env(&[OFF], path, &[]);
    assert_eq!(plain_status, Some(0), "unlinked run failed; stderr: {err}");
    (linked, plain)
}

/// A guest calling back into itself through a host `qsort`, both ways.
///
/// The interesting one: the comparator is entered from *inside* a host
/// function, so the run loop is re-entered underneath a bridged call — which is
/// exactly the arrangement where a link that should not have been taken would
/// resume the wrong frame.
#[test]
fn a_host_callback_into_guest_code_is_unaffected() {
    let Some(path) = qsort_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (linked, plain) = both_ways(&path);
    assert_eq!(linked, plain);
    assert!(linked.contains("12345789 calls=yes"), "{linked:?}");
}

/// `setjmp`/`longjmp`, both ways.
///
/// A `longjmp` is a bridged call that moves `rip` instead of returning — the
/// case that asks to be linked (see `bridge::resume`) — and its whole content
/// is that execution continues somewhere the runtime chose. If linking resumed
/// the address the bridge was *entered* at rather than the one it left behind,
/// this is the program that would say so.
#[test]
fn a_longjmp_out_of_a_bridged_call_is_unaffected() {
    let Some(path) = setjmp_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (linked, plain) = both_ways(&path);
    assert_eq!(linked, plain);
    assert!(!linked.is_empty(), "the fixture printed nothing");
}

/// Several guest threads, both ways.
///
/// The link table is per thread and published into a context that is a local of
/// whoever entered the guest, so a program with four of them is what says the
/// two are actually paired — a table shared between threads would show up as
/// one thread entering another's translation at the wrong address.
#[test]
fn guest_threads_are_unaffected() {
    let Some(path) = thread_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (linked, plain) = both_ways(&path);
    // As *sets* of lines: four threads print as they finish, so which order the
    // `thread N counter` lines come out in is the scheduler's business and not
    // this runtime's. Everything else about the two runs still has to match.
    let lines = |text: &str| {
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        lines.sort();
        lines
    };
    assert_eq!(lines(&linked), lines(&plain));
    for id in 0..4 {
        assert!(
            linked.contains(&format!("joined {id} -> 100{id}\n")),
            "{linked:?}"
        );
    }
}

/// And regions really do link: the counters say how many entries never reached
/// the run loop.
///
/// Asserted as a *share* rather than a count because the count is a property of
/// the program and the share is a property of the mechanism. Anything above a
/// clear majority means the ordinary exits — a `ret` out of a region, a call to
/// one — are being answered in translated code, which is the whole claim.
///
/// The fixture is the one whose time goes into leaving regions ([`LINK_C`]);
/// asking this of a start-up-dominated program would measure how much of it is
/// cold, since a region entered for the first time has nothing to link to yet.
#[test]
fn most_region_entries_never_reach_the_run_loop() {
    let Some(path) = link_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, _, err) = run_guest_env(&[("ROSETTA3_STATS", "1")], &path, &[]);
    assert_eq!(status, Some(0), "stderr: {err}");

    let line = err
        .lines()
        .find(|line| line.contains(" linked ("))
        .unwrap_or_else(|| panic!("no linked-entry line in the stats; {err}"));
    // `rosetta3: stats  N linked (P%), M through the run loop`
    let share: f64 = line
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once("%)"))
        .and_then(|(percent, _)| percent.parse().ok())
        .unwrap_or_else(|| panic!("could not read the share out of {line:?}"));
    assert!(
        share > 50.0,
        "only {share}% of region entries linked; {line}"
    );

    // And with the mechanism off, none of them do — which is what says the
    // number above is measuring linking rather than counting entries.
    let (_, _, err) = run_guest_env(&[("ROSETTA3_STATS", "1"), OFF], &path, &[]);
    let line = err
        .lines()
        .find(|line| line.contains(" linked ("))
        .unwrap_or_else(|| panic!("no linked-entry line in the stats; {err}"));
    assert!(line.contains("0 linked (0.00%)"), "{line}");
}
