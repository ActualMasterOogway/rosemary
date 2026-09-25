mod common;

use common::{
    link_fixture, polylink_fixture, qsort_fixture, run_guest_env, setjmp_fixture, thread_fixture,
};

const OFF: (&str, &str) = ("ROSETTA3_NO_BLOCK_LINK", "1");

const UNPINNED: (&str, &str) = ("ROSETTA3_NO_PINNED_LINK", "1");

const PER_SITE: (&str, &str) = ("ROSETTA3_INLINE_LINK_PROBES", "1");

const NO_CELLS: (&str, &str) = ("ROSETTA3_NO_LINK_CELLS", "1");

const CALLRET: (&str, &str) = ("ROSETTA3_CALL_RETURN", "1");

const LINK_MODES: [(&str, &[(&str, &str)]); 6] = [
    ("pinned links, one probe per region", &[]),
    ("pinned links, a probe per exit site", &[PER_SITE]),
    ("host-resolved links", &[UNPINNED]),
    ("static edges through the shared table", &[NO_CELLS]),
    ("returns through the call-return ring", &[CALLRET]),
    ("no linking", &[OFF]),
];

fn run_in_each_link_mode(path: &str) -> Vec<(&'static str, String)> {
    LINK_MODES
        .iter()
        .map(|&(name, env)| {
            let (status, out, err) = run_guest_env(env, path, &[]);
            assert_eq!(status, Some(0), "the {name} run failed; stderr: {err}");
            (name, out)
        })
        .collect()
}

fn assert_same_output_in_each_link_mode(path: &str) -> String {
    let runs = run_in_each_link_mode(path);
    let (first, rest) = runs.split_first().expect("LINK_MODES is not empty");
    for (name, out) in rest {
        assert_eq!(
            &first.1, out,
            "{name} disagreed with {} about the program",
            first.0
        );
    }
    first.1.clone()
}

#[test]
fn a_host_callback_into_guest_code_is_unaffected() {
    let Some(path) = qsort_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let out = assert_same_output_in_each_link_mode(&path);
    assert!(out.contains("12345789 calls=yes"), "{out:?}");
}

#[test]
fn a_longjmp_out_of_a_bridged_call_is_unaffected() {
    let Some(path) = setjmp_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let out = assert_same_output_in_each_link_mode(&path);
    assert!(!out.is_empty(), "the fixture printed nothing");
}

#[test]
fn guest_threads_are_unaffected() {
    let Some(path) = thread_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let runs = run_in_each_link_mode(&path);
    // As *sets* of lines: four threads print as they finish, so which order the
    // `thread N counter` lines come out in is the scheduler's business and not
    // this runtime's. Everything else about the runs still has to match.
    let lines = |text: &str| {
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        lines.sort();
        lines
    };
    let (first, rest) = runs.split_first().expect("LINK_MODES is not empty");
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

#[test]
fn a_region_with_four_successors_is_unaffected() {
    let Some(path) = polylink_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let out = assert_same_output_in_each_link_mode(&path);
    assert!(out.starts_with("acc="), "{out:?}");
}

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

#[test]
fn a_static_edge_is_taken_through_a_cell_of_its_own() {
    let Some(path) = polylink_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };

    // `rosetta3: stats  cache ...  cells N live, M waiting (...)  links ...`
    let cells = |env: &[(&str, &str)]| -> (usize, usize) {
        let (status, _, err) = run_guest_env(env, &path, &[]);
        assert_eq!(status, Some(0), "stderr: {err}");
        let line = err
            .lines()
            .find(|line| line.contains(" cells "))
            .unwrap_or_else(|| panic!("no cache line in the stats; {err}"));
        let after = line.split_once(" cells ").expect("just matched").1;
        let (live, rest) = after.split_once(" live, ").expect("malformed cells item");
        let (waiting, _) = rest.split_once(" waiting").expect("malformed cells item");
        (
            live.parse().unwrap_or_else(|_| panic!("{line:?}")),
            waiting.parse().unwrap_or_else(|_| panic!("{line:?}")),
        )
    };

    let (live, waiting) = cells(&[("ROSETTA3_STATS", "1")]);
    assert!(live > 0, "no link cells were installed");
    assert_eq!(waiting, 0, "{waiting} cell(s) never found their target");

    // With the ring on, each call site additionally gets a `CallReturn` cell for the
    // address it comes back to, so the count rises above the static edges alone. That
    // the two knobs move the same number in the same direction is what says the ring
    // is really riding on cells.
    let (with_ring, _) = cells(&[("ROSETTA3_STATS", "1"), CALLRET]);
    assert!(
        with_ring > live,
        "turning the call-return ring on added no cells to the {live} static edge(s); \
         returns are not being predicted"
    );

    // And with cells off there are none at all - which is what says the numbers
    // above measure the mechanism rather than counting exits.
    assert_eq!(cells(&[("ROSETTA3_STATS", "1"), NO_CELLS]), (0, 0));
}
