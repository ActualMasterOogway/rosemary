//! M6.3 acceptance: messages sent by the guest.
//!
//! The direction M6.2 left open. [`objc.rs`](objc.rs) messages a guest class
//! from the *host*, which proves realization and thunking; nothing there sends a
//! message from guest code, and until this milestone a Cocoa guest linked
//! cleanly and stopped at its first one.
//!
//! ## Why these run through the driver
//!
//! Every test here is a whole process, and the assertion is against what the
//! **same binary prints natively**. That is the only specification of an
//! Objective-C program worth having: the runtime is the host's either way, so
//! any divergence is this runtime's doing. Rosetta 2 is still present, so the
//! comparison is against the identical x86_64 file rather than a recompilation —
//! and when it is not available the test skips rather than inventing an expected
//! string.
//!
//! It also side-steps the constraint `objc.rs` is built around: a process runs
//! one Objective-C guest, because class names are global to the host runtime. A
//! test that is a process has that to itself.

mod common;

/// Run a fixture both ways and require the output to agree exactly.
///
/// Returns the shared output so a caller can make a further claim about it.
fn agrees(path: &str, args: &[&str]) -> Option<String> {
    let native = std::process::Command::new(path).args(args).output().ok()?;
    native.status.code()?;
    let expected = String::from_utf8_lossy(&native.stdout).into_owned();

    let (status, stdout, stderr) = common::run_guest(path, args);
    assert_eq!(stdout, expected, "guest output differs; stderr: {stderr}");
    assert_eq!(status, native.status.code(), "exit status differs; stderr: {stderr}");
    Some(stdout)
}

/// The gate: an Objective-C program that sends messages runs, and prints what it
/// prints natively.
///
/// Sixteen claims in one process — see [`common::OBJC_MESSAGES_M`] for what each
/// line is for. Comparing the whole output rather than asserting line by line is
/// deliberate: a runtime that got `super` wrong or resolved an override on the
/// wrong class produces *plausible* output, and only the diff against the real
/// thing says so.
#[test]
fn an_objective_c_program_sends_messages_and_prints_what_it_should() {
    let Some(path) = common::objc_messages_fixture("objc_messages", &[]) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let Some(out) = agrees(&path, &[]) else { skip!("cannot run x86_64 binaries natively") };

    // Two lines carry claims that are easy to satisfy by accident, so they are
    // named as well as diffed.
    assert!(out.contains("load 11\n"), "+load ran in the wrong order or not at all: {out}");
    assert!(out.contains("identity 1 0\n"), "a class coming back was not the guest's: {out}");
}

/// The same program at `-O2`, where clang **tail-jumps** to `objc_msgSend`.
///
/// A different path through the runtime for identical output: there is no return
/// address to write, because the jump left the caller's on the stack, and a shim
/// that pushed one anyway would return the method's result to the wrong frame.
/// The optimiser also rewrites `[X alloc]` into `objc_alloc` and `[[X alloc]
/// init]` into `objc_alloc_init`, which are entry points rather than messages.
#[test]
fn the_same_program_optimised_takes_the_tail_call_path() {
    let Some(path) = common::objc_messages_fixture("objc_messages_o2", &["-O2"]) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let Some(_) = agrees(&path, &[]) else { skip!("cannot run x86_64 binaries natively") };
}

/// Every import an Objective-C program makes is described.
///
/// The milestone-2 claim, applied to the family M6.3 added: nothing may be
/// [`Classification::Unknown`], because unknown is the one outcome that becomes a
/// wrong value in a register rather than a diagnostic. The Objective-C surface is
/// where this is easiest to lose — a class reference is one import per class the
/// program mentions.
///
/// [`Classification::Unknown`]: rosetta3::abi::Classification
#[test]
fn every_objective_c_import_is_classified() {
    use rosetta3::abi::Classification;

    let Some(path) = common::objc_messages_fixture("objc_classify", &[]) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let program = rosetta3::Program::load(&path, &[]).expect("linking");
    let unknown: Vec<&str> = program
        .classify_imports()
        .into_iter()
        .filter(|(_, c)| *c == Classification::Unknown)
        .map(|(name, _)| name)
        .collect();
    assert!(unknown.is_empty(), "undescribed imports: {unknown:?}");
}

/// `objc_msgSend` is never inlined by the lifter.
///
/// It has no signature to inline *with* — it has whichever signature the method
/// has — so promoting it would call the host's own trampoline with the guest's
/// registers, which is a crash inside somebody else's assembly. The rule that
/// prevents it is that a shimmed symbol is never promoted, and this is the
/// assertion that it really covers this family.
#[test]
fn the_message_send_family_is_never_promoted_to_a_fast_import() {
    use rosetta3::abi::Classification;

    for symbol in ["_objc_msgSend", "_objc_msgSendSuper2", "_objc_alloc_init", "_class_getName"] {
        assert_eq!(rosetta3::abi::classify(0, symbol), Classification::Slow, "{symbol}");
    }
}

/// The three messages that must be refused, each as a **named** stop.
///
/// Not failures to fix by trying harder: a variadic method's arguments are in
/// places the encoding does not describe, an aggregate return is M2's refusal
/// arriving on schedule, and `@throw` is M5's unwinder with a different
/// personality routine. What this checks is that each one says so, with the
/// program's output up to that point intact — a refusal that crashed instead
/// would print nothing.
#[test]
fn each_refused_message_stops_with_a_reason() {
    let Some(path) = common::objc_refusals_fixture() else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };

    // The same binary with no argument does none of the three and must run.
    let (status, stdout, stderr) = common::run_guest(&path, &[]);
    assert_eq!(status, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, "before\nafter\n");

    for (which, needle) in
        [("variadic", "variadic method"), ("aggregate", "aggregate"), ("throw", "personality")]
    {
        let (status, stdout, stderr) = common::run_guest(&path, &[which]);
        assert_ne!(status, Some(0), "{which} was not refused");
        assert_eq!(stdout, "before\n", "{which} ran past the refusal");
        assert!(stderr.contains(needle), "{which}: expected {needle:?}, got {stderr:?}");
    }
}

/// An import nothing satisfied fails **by name**, not by address.
///
/// Before M6.3 the diagnostic for this was `no guest code at 0x0`: the slot the
/// binder zeroed was not registered, so the recompiler followed the `__stubs`
/// trampoline into it. The whole content of the failure is a symbol name, and
/// that is what it says now. Provoked with a symbol the native provider denies
/// on purpose — the host's image list is not the guest's.
#[test]
fn an_unsatisfied_import_names_itself_when_it_is_called() {
    const SRC: &str = r#"
    #include <stdio.h>
    #include <mach-o/dyld.h>
    int main(void) { printf("before\n"); printf("images %u\n", _dyld_image_count()); return 0; }
    "#;
    let Some(path) = common::build_x86_64("unsatisfied", SRC, &[]) else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, stdout, stderr) = common::run_guest(&path, &[]);
    assert_ne!(status, Some(0));
    assert_eq!(stdout, "before\n");
    assert!(stderr.contains("_dyld_image_count"), "{stderr}");
    assert!(stderr.contains("unsatisfied import"), "{stderr}");
}

/// The slow-import policy takes the same program to the same place.
///
/// M2's falsifiable claim, applied to M6.3: fast imports are an optimisation of
/// slow ones. Message sending is unaffected either way — `objc_msgSend` is
/// shimmed, so it is a VM exit under both policies — and this says the rest of
/// the program's imports really are interchangeable.
#[test]
fn the_slow_import_policy_sends_the_same_messages() {
    let Some(path) = common::objc_messages_fixture("objc_messages_slow", &[]) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let (fast_status, fast, _) = common::run_guest(&path, &[]);
    let (slow_status, slow, stderr) = common::run_guest_with(&["--slow"], &path, &[]);
    assert_eq!(fast, slow, "stderr: {stderr}");
    assert_eq!(fast_status, slow_status);
}
