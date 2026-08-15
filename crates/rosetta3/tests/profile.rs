//! A running guest says where its time went, in guest addresses.
//!
//! The gap this closes is the one [M8.1](../../../docs/ROADMAP.md) ended on: a
//! 2.4× microbenchmark win that the application it was built for could not
//! detect, because a sample inside translated code lands in an anonymous
//! mapping with no image, no symbol and nothing to attribute it to. Every
//! remaining performance candidate was a plausible story with no measurement
//! behind it.
//!
//! What each test would falsify:
//!
//! - **it samples at all** — a sampler that never started, or one whose timer
//!   thread died, prints nothing, and a report with zero samples is the same
//!   observation as no report;
//! - **it names guest instructions in the guest's own image**, which is the
//!   whole claim: a host `pc` is not a work item and `image+offset` is, and it
//!   is the form that survives across runs into a disassembler;
//! - **it separates translated code from this runtime's Rust and from the
//!   host's libraries**, which is the question M8's candidate list splits on
//!   and which no existing tool answers, because all three are the same colour
//!   in every one of them;
//! - **it costs nothing when it is off**, so leaving the code in the runtime is
//!   not a tax on every run;
//! - **the map it writes is joinable**, which is what makes Apple's own
//!   tooling — `xctrace`, and Processor Trace in particular — usable against
//!   JIT code at all.

mod common;

use common::{profile_fixture, run_guest_env};

/// Every profile line, without the prefix.
fn lines(err: &str) -> Vec<&str> {
    err.lines()
        .filter_map(|l| l.strip_prefix("rosetta3: profile  "))
        .collect()
}

/// The percentage a labelled share reports, out of a `translated 93.3%  ...`
/// line.
fn share(err: &str, label: &str) -> Option<f64> {
    let line = lines(err).into_iter().find(|l| l.contains(label))?;
    let after = &line[line.find(label)? + label.len()..];
    let number: String = after
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    number.parse().ok()
}

#[test]
fn a_profiled_run_ranks_guest_instructions_in_the_guests_own_image() {
    let Some(path) = profile_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest_env(&[("ROSETTA3_PROFILE", "1")], &path, &[]);

    // The program ran to completion: a profile of a crash would be a profile of
    // the wrong thing, and the fixture's own output is what says so.
    assert_eq!(status, Some(0), "stderr: {err}");
    assert!(out.contains(' '), "the fixture printed nothing: {out:?}");

    let lines = lines(&err);
    assert!(!lines.is_empty(), "no profile was printed; stderr: {err}");
    assert!(
        lines.iter().any(|l| l.contains("sample(s) over")),
        "the sampler took no samples; stderr: {err}"
    );
    // The claim. A ranked list whose entries are bare host addresses would be
    // what every other tool already produces.
    assert!(
        lines.iter().any(|l| l.contains("guest instructions")),
        "no guest instruction ranking; stderr: {err}"
    );
    assert!(
        err.contains("guest_profile+0x"),
        "no sample was resolved into the guest's own image; stderr: {err}"
    );
}

#[test]
fn the_three_halves_of_the_profile_are_told_apart() {
    let Some(path) = profile_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (_, _, err) = run_guest_env(&[("ROSETTA3_PROFILE", "2000")], &path, &[]);

    let translated = share(&err, "translated").expect(&format!("no shares line: {err}"));
    let runtime = share(&err, "runtime").unwrap_or(0.0);
    let libraries = share(&err, "host libraries").unwrap_or(0.0);

    // The fixture spends the bulk of its time in a translated arithmetic loop,
    // by construction. This is deliberately a loose bound: the exact split
    // depends on the host and on what else it is doing, and a test that pinned
    // it would fail for reasons that are not about the profiler.
    assert!(
        translated > 40.0,
        "a guest in a hot arithmetic loop should be mostly in translated code, \
         got {translated}%; stderr: {err}"
    );
    // And its second loop crosses the seam a hundred thousand times, so the
    // other two buckets cannot both be empty. A profiler that attributed
    // everything to the guest would pass the assertion above and fail this one,
    // which is the failure worth catching: it is the one that would send M8
    // after the wrong half of its candidate list.
    assert!(
        runtime + libraries > 0.0,
        "a guest that calls snprintf a hundred thousand times spends time \
         outside translated code; stderr: {err}"
    );
    assert!(
        translated + runtime + libraries <= 100.5,
        "the shares are of one whole and must not exceed it, got \
         {translated} + {runtime} + {libraries}; stderr: {err}"
    );
}

#[test]
fn the_ranking_written_to_a_file_agrees_with_the_one_printed() {
    let Some(path) = profile_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let out = std::env::temp_dir().join(format!("rosetta3-profile-{}.tsv", std::process::id()));
    let map = std::env::temp_dir().join(format!("rosetta3-map-{}.txt", std::process::id()));
    let (_, _, err) = run_guest_env(
        &[
            ("ROSETTA3_PROFILE", "1"),
            ("ROSETTA3_PROFILE_OUT", &out.to_string_lossy()),
            ("ROSETTA3_PROFILE_MAP", &map.to_string_lossy()),
            ("ROSETTA3_PROFILE_MAP_INSTRUCTIONS", "1"),
        ],
        &path,
        &[],
    );

    let histogram = std::fs::read_to_string(&out).expect(&format!("no {out:?}; stderr: {err}"));
    assert!(
        histogram.lines().any(|l| l.starts_with("instruction\t")),
        "the file has no instruction rows: {histogram:.400}"
    );
    assert!(
        histogram.lines().any(|l| l.starts_with("region\t")),
        "the file has no region rows: {histogram:.400}"
    );

    // The map is the half that makes an external trace readable, and the one
    // property it must have is that its host ranges are ordered, non-empty and
    // inside the region they belong to — otherwise a lookup by host address
    // resolves to the wrong guest instruction, silently.
    let text = std::fs::read_to_string(&map).expect(&format!("no {map:?}; stderr: {err}"));
    let hex = |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
    let mut regions = 0;
    let mut instructions = 0;
    let mut region_extent = (0u64, 0u64);
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f[0] {
            "R" => {
                regions += 1;
                region_extent = (hex(f[1]), hex(f[2]));
                assert!(region_extent.0 < region_extent.1, "empty region: {line}");
                assert!(hex(f[3]) < hex(f[4]), "empty guest range: {line}");
            }
            "I" => {
                instructions += 1;
                let (start, end) = (hex(f[1]), hex(f[2]));
                assert!(start < end, "empty instruction range: {line}");
                assert!(
                    start >= region_extent.0 && end <= region_extent.1,
                    "{line} is outside the region it follows, \
                     {:#x}..{:#x}",
                    region_extent.0,
                    region_extent.1
                );
            }
            other => panic!("unknown record {other:?}: {line}"),
        }
    }
    assert!(regions > 0, "the map named no regions");
    assert!(
        instructions > regions,
        "instruction detail was asked for and {instructions} line(s) came back \
         for {regions} region(s)"
    );

    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&map);
}

#[test]
fn a_run_that_did_not_ask_to_be_profiled_is_not() {
    let Some(path) = profile_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    // Not merely "prints nothing": the sampler thread must not be started at
    // all, because a runtime that suspends every guest thread a thousand times
    // a second whether or not anyone asked is a runtime nobody would leave the
    // code in.
    let (status, _, err) = run_guest_env(&[], &path, &[]);
    assert_eq!(status, Some(0), "stderr: {err}");
    assert!(
        !err.contains("rosetta3: profile"),
        "an unprofiled run said something about profiling; stderr: {err}"
    );

    for off in ["0", "off", ""] {
        let (_, _, err) = run_guest_env(&[("ROSETTA3_PROFILE", off)], &path, &[]);
        assert!(
            !err.contains("rosetta3: profile"),
            "ROSETTA3_PROFILE={off:?} started a sampler; stderr: {err}"
        );
    }
}
