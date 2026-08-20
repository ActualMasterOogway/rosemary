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
//!
//! There are **three** shapes of linking to hold to those claims. A host-resolved
//! link (`ROSETTA3_NO_PINNED_LINK=1`) hands the successor back to
//! `cache::follow_links`; a pinned link branches straight into it with part of the
//! guest register file still in host registers (`rosetta3::jit::pinned_link_abi`),
//! and is the default; and either resolves its successor from a probe shared by
//! the region or from one per exit site (`ROSETTA3_INLINE_LINK_PROBES=1`). The
//! first claim is asserted for all of them and *between* them, since a pinned ABI
//! moves where a guest register lives and a per-site probe moves where a successor
//! is resolved - the two changes most able to corrupt one. The second is asserted
//! of the host-resolved shape only, because a pinned link is a branch inside
//! translated code and deliberately increments nothing.

mod common;

use common::{
    link_fixture, polylink_fixture, qsort_fixture, run_guest_env, setjmp_fixture, thread_fixture,
};

/// The environment that turns the mechanism off. See `rosetta3::jit`.
const OFF: (&str, &str) = ("ROSETTA3_NO_BLOCK_LINK", "1");

/// Links, but resolved by the host rather than by a branch - the shape block
/// linking was built in, and the one whose entries are countable.
const UNPINNED: (&str, &str) = ("ROSETTA3_NO_PINNED_LINK", "1");

/// One probe per exit site rather than one shared by the region.
const PER_SITE: (&str, &str) = ("ROSETTA3_INLINE_LINK_PROBES", "1");

/// Every shape of the mechanism, named, as the environment that selects it.
///
/// Four rather than two because each pair fails differently: unlinked against
/// linked is about *who decides* what runs next, host-resolved against pinned is
/// about *where a guest register lives* while it does, and shared against per-site
/// is about *which branch* carries a region's successors.
const WAYS: [(&str, &[(&str, &str)]); 4] = [
    ("pinned links, one probe per region", &[]),
    ("pinned links, a probe per exit site", &[PER_SITE]),
    ("host-resolved links", &[UNPINNED]),
    ("no linking", &[OFF]),
];

/// Run `path` every way, and return each run's output beside the name of the shape
/// that produced it.
fn every_way(path: &str) -> Vec<(&'static str, String)> {
    WAYS.iter()
        .map(|&(name, env)| {
            let (status, out, err) = run_guest_env(env, path, &[]);
            assert_eq!(status, Some(0), "the {name} run failed; stderr: {err}");
            (name, out)
        })
        .collect()
}

/// [`every_way`] for a program whose output is deterministic: every run must be
/// byte-identical. Returns that output.
fn one_answer(path: &str) -> String {
    let runs = every_way(path);
    let (first, rest) = runs.split_first().expect("WAYS is not empty");
    for (name, out) in rest {
        assert_eq!(
            &first.1, out,
            "{name} disagreed with {} about the program",
            first.0
        );
    }
    first.1.clone()
}

/// A guest calling back into itself through a host `qsort`, every way.
///
/// The interesting one: the comparator is entered from *inside* a host
/// function, so the run loop is re-entered underneath a bridged call - which is
/// exactly the arrangement where a link that should not have been taken would
/// resume the wrong frame.
#[test]
fn a_host_callback_into_guest_code_is_unaffected() {
    let Some(path) = qsort_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let out = one_answer(&path);
    assert!(out.contains("12345789 calls=yes"), "{out:?}");
}

/// `setjmp`/`longjmp`, every way.
///
/// A `longjmp` is a bridged call that moves `rip` instead of returning - the
/// case that asks to be linked (see `bridge::resume`) - and its whole content
/// is that execution continues somewhere the runtime chose. If linking resumed
/// the address the bridge was *entered* at rather than the one it left behind,
/// this is the program that would say so.
#[test]
fn a_longjmp_out_of_a_bridged_call_is_unaffected() {
    let Some(path) = setjmp_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let out = one_answer(&path);
    assert!(!out.is_empty(), "the fixture printed nothing");
}

/// Several guest threads, every way.
///
/// The link table is per thread and published into a context that is a local of
/// whoever entered the guest, so a program with four of them is what says the
/// two are actually paired - a table shared between threads would show up as
/// one thread entering another's translation at the wrong address.
#[test]
fn guest_threads_are_unaffected() {
    let Some(path) = thread_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let runs = every_way(&path);
    // As *sets* of lines: four threads print as they finish, so which order the
    // `thread N counter` lines come out in is the scheduler's business and not
    // this runtime's. Everything else about the runs still has to match.
    let lines = |text: &str| {
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        lines.sort();
        lines
    };
    let (first, rest) = runs.split_first().expect("WAYS is not empty");
    for (name, out) in rest {
        assert_eq!(
            lines(&first.1),
            lines(out),
            "{name} disagreed with {} about the program",
            first.0
        );
    }
    for id in 0..4 {
        assert!(
            first.1.contains(&format!("joined {id} -> 100{id}\n")),
            "{:?}",
            first.1
        );
    }
}

/// A region whose exits do **not** all go to the same place, every way.
///
/// [`LINK_C`] has one successor per exit site, so it agrees with itself whether a
/// region's successors are reached through one branch or four. `POLYLINK_C` is the
/// fixture where that distinction exists at all, which makes it the one that can
/// catch a per-site probe resuming at the wrong address.
///
/// [`LINK_C`]: common::LINK_C
/// [`POLYLINK_C`]: common::POLYLINK_C
#[test]
fn a_region_with_four_successors_is_unaffected() {
    let Some(path) = polylink_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let out = one_answer(&path);
    assert!(out.starts_with("acc="), "{out:?}");
}

/// The probes on a polymorphic region are taken, and taken for every kind of exit
/// it has.
///
/// Stated per kind because a whole-program share hides the interesting failure: a
/// change that stopped `ret` linking while leaving direct calls alone would still
/// show a high aggregate.
///
/// Asked of the **host-resolved** shape for the same reason
/// [`most_region_entries_never_reach_the_run_loop`] is: counting the exits that
/// declined is free either way, but the denominator - every exit of that kind - is
/// only counted when the host sees each link. Under a pinned ABI the report prints
/// the counts and no share, and this asks the question of the shape that can
/// answer it. Both probe shapes are checked, because the decline *rate* is a
/// property of the table and must not move when only the branch does.
#[test]
fn every_linkable_exit_kind_finds_its_successor() {
    let Some(path) = polylink_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };

    // `rosetta3: stats  probes declined  <kind> P% of N  <kind> P% of N ...`
    let declined = |env: &[(&str, &str)]| -> Vec<(String, f64)> {
        let (status, _, err) = run_guest_env(env, &path, &[]);
        assert_eq!(status, Some(0), "stderr: {err}");
        let line = err
            .lines()
            .find(|line| line.contains(" probes declined  "))
            .unwrap_or_else(|| panic!("no probes-declined line in the stats; {err}"))
            .split_once(" probes declined  ")
            .expect("just matched")
            .1
            .to_owned();
        // Items are two spaces apart; a label may hold single ones.
        line.split("  ")
            .map(|item| {
                let (head, _total) = item
                    .rsplit_once(" of ")
                    .unwrap_or_else(|| panic!("malformed item {item:?} in {line:?}"));
                let (kind, share) = head
                    .rsplit_once(' ')
                    .unwrap_or_else(|| panic!("malformed item {item:?} in {line:?}"));
                let share: f64 = share
                    .trim_end_matches('%')
                    .parse()
                    .unwrap_or_else(|_| panic!("malformed share in {item:?}"));
                (kind.to_owned(), share)
            })
            .collect()
    };

    for (name, env) in [
        (
            "one probe per region",
            vec![("ROSETTA3_STATS", "1"), UNPINNED],
        ),
        (
            "a probe per exit site",
            vec![("ROSETTA3_STATS", "1"), UNPINNED, PER_SITE],
        ),
    ] {
        let kinds = declined(&env);
        assert!(!kinds.is_empty(), "{name}: no linkable exits were counted");
        for (kind, share) in &kinds {
            assert!(
                *share < 25.0,
                "{name}: {share}% of `{kind}` exits declined their probe; {kinds:?}"
            );
        }
    }

    // With the mechanism off every linkable exit declines, which is what says the
    // numbers above measure linking rather than counting exits.
    let off = declined(&[("ROSETTA3_STATS", "1"), UNPINNED, OFF]);
    assert!(!off.is_empty(), "no linkable exits were counted");
    for (kind, share) in &off {
        assert!(
            (*share - 100.0).abs() < 0.01,
            "linking is off but only {share}% of `{kind}` exits declined; {off:?}"
        );
    }
}

/// And regions really do link: the counters say how many entries never reached
/// the run loop.
///
/// Asked of the **host-resolved** shape, which is the one that can answer: a
/// pinned link is a branch and counts nothing. What stands in for this under a
/// pinned ABI is `most_entries_are_chains_under_a_pinned_abi` below.
///
/// Asserted as a *share* rather than a count because the count is a property of
/// the program and the share is a property of the mechanism. Anything above a
/// clear majority means the ordinary exits - a `ret` out of a region, a call to
/// one - are being answered in translated code, which is the whole claim.
///
/// The fixture is the one whose time goes into leaving regions ([`LINK_C`]);
/// asking this of a start-up-dominated program would measure how much of it is
/// cold, since a region entered for the first time has nothing to link to yet.
#[test]
fn most_region_entries_never_reach_the_run_loop() {
    let Some(path) = link_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, _, err) = run_guest_env(&[("ROSETTA3_STATS", "1"), UNPINNED], &path, &[]);
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

    // And with the mechanism off, none of them do - which is what says the
    // number above is measuring linking rather than counting entries.
    let (_, _, err) = run_guest_env(&[("ROSETTA3_STATS", "1"), OFF, UNPINNED], &path, &[]);
    let line = err
        .lines()
        .find(|line| line.contains(" linked ("))
        .unwrap_or_else(|| panic!("no linked-entry line in the stats; {err}"));
    assert!(line.contains("0 linked (0.00%)"), "{line}");
}

/// The pinned shape's version of the claim above: a link is a branch, so the
/// host sees *chains* rather than regions and the entry count collapses.
///
/// Stated as a ratio against the host-resolved run of the same fixture, because
/// that run counts every region entry and this one counts every entry into
/// translated code. The gap between the two is exactly the region entries that
/// never returned to Rust - and if a refactor stopped pinned links from
/// happening, the two numbers would converge and this fails.
#[test]
fn a_pinned_abi_turns_entries_into_chains() {
    let Some(path) = link_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };

    let entries = |env: &[(&str, &str)]| -> f64 {
        let (status, _, err) = run_guest_env(env, &path, &[]);
        assert_eq!(status, Some(0), "stderr: {err}");
        let line = err
            .lines()
            .find(|line| line.contains(" region entr(ies)"))
            .unwrap_or_else(|| panic!("no entry line in the stats; {err}"));
        line.split_whitespace()
            .nth(2)
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("could not read the entry count out of {line:?}"))
    };

    let pinned = entries(&[("ROSETTA3_STATS", "1")]);
    let host_resolved = entries(&[("ROSETTA3_STATS", "1"), UNPINNED]);
    assert!(
        pinned * 2.0 < host_resolved,
        "a pinned run entered translated code {pinned} times against the host-resolved \
         run's {host_resolved}; links are not being taken as branches"
    );
}
