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
    assert_eq!(
        status,
        native.status.code(),
        "exit status differs; stderr: {stderr}"
    );
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
    let Some(out) = agrees(&path, &[]) else {
        skip!("cannot run x86_64 binaries natively")
    };

    // Two lines carry claims that are easy to satisfy by accident, so they are
    // named as well as diffed.
    assert!(
        out.contains("load 11\n"),
        "+load ran in the wrong order or not at all: {out}"
    );
    assert!(
        out.contains("identity 1 0\n"),
        "a class coming back was not the guest's: {out}"
    );
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
    let Some(_) = agrees(&path, &[]) else {
        skip!("cannot run x86_64 binaries natively")
    };
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
/// registers, which is a crash inside somebody else's assembly.
///
/// `objc_alloc_init` is here for a different reason and it is the more subtle
/// one: it *has* an ordinary `id(Class)` signature, and it still may not be
/// inlined, because it sends `init` — which for a guest class is a thunk back
/// into the JIT, entered against a
/// [stale guest stack pointer](rosetta3::objc::fast). Nothing in its signature
/// says so, which is why the rule is a list of names rather than a property of
/// the types.
#[test]
fn the_message_send_family_is_never_promoted_to_a_fast_import() {
    use rosetta3::abi::Classification;

    for symbol in ["_objc_msgSend", "_objc_msgSendSuper2", "_objc_alloc_init"] {
        assert_eq!(
            rosetta3::abi::classify(0, symbol),
            Classification::Slow,
            "{symbol}"
        );
    }
}

/// A runtime entry point that cannot reach guest code *is* inlined.
///
/// The other half of the rule above, and the reason it is worth having its own
/// test: "shimmed" and "slow" used to be the same statement, and this is where
/// they came apart. `class_getName` is shimmed because the native provider
/// denies the whole `_class_` family so [`rosetta3::objc::dispatch`] can claim
/// the sends — not because calling the host's would be wrong. It reads a name
/// out of a class and cannot dispatch, so it is inlined against a substituting
/// function that translates the class and calls the host's.
///
/// Named as a classification rather than as a benchmark, because the failure it
/// guards against is silent: a runtime entry point that quietly went back to
/// being a bridged call would print exactly the same output.
#[test]
fn a_runtime_entry_point_that_cannot_dispatch_is_a_fast_import() {
    use rosetta3::abi::Classification;

    for symbol in ["_class_getName", "_object_getClass", "_sel_registerName"] {
        assert_eq!(
            rosetta3::abi::classify(0, symbol),
            Classification::Fast,
            "{symbol}"
        );
    }
}

/// Two of the three former refusals now *work*, and the third still says why.
///
/// This test used to assert all three were refused, and two of them moving is
/// the whole of what [`rosetta3::abi::aggregate`] and the variadic re-send are
/// for. What has not moved is `@throw`, which is M5's unwinder with a different
/// personality routine — and what matters about it is unchanged: it says so,
/// with the program's output up to that point intact, rather than crashing.
#[test]
fn an_aggregate_return_and_a_variadic_method_both_work_and_throw_still_names_itself() {
    let Some(path) = common::objc_refusals_fixture() else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };

    // The same binary with no argument does none of the three and must run.
    let (status, stdout, stderr) = common::run_guest(&path, &[]);
    assert_eq!(status, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, "before\nafter\n");

    // `[s appendFormat:@"%d", 3]` — the format is rendered here and the method
    // re-sent with the result, so the ellipsis the encoding never described is
    // empty by the time the host sees the call.
    let (status, stdout, stderr) = common::run_guest(&path, &["variadic"]);
    assert_eq!(status, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, "before\nappended 3\nafter\n");

    // `-[NSString rangeOfString:]` returns an `NSRange` by value: two integer
    // eightbytes to the guest, two `x` registers to the host. "world" begins at
    // six, and a marshaller that returned the wrong register would say zero.
    let (status, stdout, stderr) = common::run_guest(&path, &["aggregate"]);
    assert_eq!(status, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, "before\nrange 6\nafter\n");

    let (status, stdout, stderr) = common::run_guest(&path, &["throw"]);
    assert_ne!(status, Some(0), "throw was not refused");
    assert_eq!(stdout, "before\n", "throw ran past the refusal");
    assert!(
        stderr.contains("personality"),
        "throw: expected a personality-routine refusal, got {stderr:?}"
    );
}

/// An import nothing satisfied fails **by name**, not by address.
///
/// Before M6.3 the diagnostic for this was `no guest code at 0x0`: the slot the
/// binder zeroed was not registered, so the recompiler followed the `__stubs`
/// trampoline into it. The whole content of the failure is a symbol name, and
/// that is what it says now. Provoked with a symbol the native provider denies
/// on purpose — dyld introspection, which the whole `__dyld_` prefix refuses so
/// that a shim has to decide what the guest is told.
///
/// The fixture used to call `_dyld_image_count`, and that stopped being a
/// denied-and-unanswered symbol when [`rosetta3::shims::runtime`] began
/// answering the image-list family for Unity's crash reporter. It moved to the
/// vmaddr slide, and *that* stopped being one too — the whole image-list family
/// is claimed now, because a guest crash reporter walks it. It is
/// `_dyld_get_prog_image_header` today. **The test moving is the correct
/// outcome each time**: the alternative is holding a shim back so a diagnostic
/// keeps its example, which would be the tail wagging the dog.
#[test]
fn an_unsatisfied_import_names_itself_when_it_is_called() {
    const SRC: &str = r#"
    #include <stdio.h>
    #include <mach-o/dyld.h>
    int main(void) {
        printf("before\n");
        printf("header %p\n", (const void *)_dyld_get_prog_image_header());
        return 0;
    }
    "#;
    let Some(path) = common::build_x86_64("unsatisfied", SRC, &[]) else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, stdout, stderr) = common::run_guest(&path, &[]);
    assert_ne!(status, Some(0));
    assert_eq!(stdout, "before\n");
    assert!(stderr.contains("_dyld_get_prog_image_header"), "{stderr}");
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

/// The **host** calls a guest method with an aggregate by value.
///
/// The direction every other aggregate test here has backwards. Guest→host is a
/// marshaller reading the guest's registers; this is a compiled trampoline
/// placing AAPCS64's `v0`–`v3` where SysV AMD64 expects an `NSRect` — which for
/// thirty-two bytes is the guest's **stack**, not any register at all.
///
/// Checked against the same binary run natively, because the specification of
/// "what does `setBounds:` receive" is what the platform does, not what this
/// runtime believes. A trampoline forwarding registers only prints garbage for
/// `bounds` and is right about everything else, which is exactly why the three
/// shapes are separate lines.
#[test]
fn the_host_calls_a_guest_method_with_an_aggregate_by_value() {
    let Some(path) = common::objc_host_aggregate_fixture() else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let Some((native_status, expected)) = common::run_native(&path) else {
        skip!("cannot run x86_64 binaries natively")
    };
    let (status, out, err) = common::run_guest(&path, &[]);

    assert_eq!(out, expected, "stderr: {err}");
    assert_eq!(status, native_status, "stderr: {err}");
    // Named as well as diffed: an NSRect that never reached the guest's stack
    // reads as whatever was there, and "0.0 0.0 0.0 0.0" is the usual answer.
    assert!(out.contains("bounds 1.5 2.5 3.5 4.5\n"), "{out}");
    assert!(out.contains("origin 6.5 7.5\n"), "{out}");
    assert!(out.contains("scale 9.25\n"), "{out}");
}

/// M7.4: a `dlopen`ed image's own Objective-C classes are realized too.
///
/// The launch-time registration pass runs over the program's images, and an
/// image the guest maps afterwards is not one of them. Everything in the plugin
/// therefore used to fail, and the failure that reached a real program first was
/// the most direct one: SDL3's Cocoa backend declares its own `NSApplication`
/// subclass, and Terraria's start-up messages it — which stopped with
/// *"the receiver is a guest address whose class the runtime never realized"*.
///
/// Checked against the same binary run natively rather than against a
/// transcript this runtime wrote down, because what `+load` prints and when is
/// the platform's answer and not this project's. The order is the part worth
/// having: `+load` runs inside `dlopen`, before the same image's C constructor.
#[test]
fn a_loaded_image_declares_classes_and_the_guest_messages_them() {
    let Some(path) = common::objc_dlopen_fixture() else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let Some((native_status, expected)) = common::run_native(&path) else {
        skip!("cannot run x86_64 binaries natively")
    };
    let (status, out, err) = common::run_guest(&path, &[]);

    assert_eq!(out, expected, "stderr: {err}");
    assert_eq!(status, native_status, "stderr: {err}");
    // Named as well as diffed, because each line fails for its own reason: the
    // first two are the ordering claim, and `plugin=` is 41 + 1 + 6, so a
    // message that silently returned zero is a different number rather than an
    // absent line.
    assert!(out.contains("plugin +load\nplugin ctor\n"), "{out}");
    assert!(out.contains("plugin=48\n"), "{out}");
}

/// The promotion really fires on a linked program, not just in a unit test.
///
/// [`rosetta3::objc::fast::promotion`] declines unless the site resolved to
/// *exactly* the address `dlsym` gives for the entry point — which is the whole
/// equivalence argument, and which nothing in this repository can check without
/// a real link. The synthetic provider defines these symbols from `dlsym`, so
/// the two agree by construction; this is the test that says so rather than
/// assuming it, because a provider that ever started wrapping one of them would
/// silently make every promotion a call to the wrong function.
///
/// Asserts a non-empty intersection as well as the property, since a program
/// that happened to import none of the promotable entries would pass a vacuous
/// version of this and say nothing.
#[test]
fn a_linked_program_really_promotes_its_runtime_entry_points() {
    let Some(path) = common::objc_messages_fixture("objc_promote", &[]) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let program = rosetta3::Program::load(&path, &[]).expect("linking");

    let mut promoted = Vec::new();
    for site in program.imports.values() {
        if rosetta3::abi::classify(site.tag, &site.symbol) != rosetta3::abi::Classification::Fast {
            continue;
        }
        let Some(p) = rosetta3::objc::fast::promotion(&site.symbol, site.resolved) else {
            continue;
        };
        assert_ne!(p.address, 0, "{}", site.symbol);
        promoted.push(site.symbol.clone());
    }
    promoted.sort();
    promoted.dedup();
    assert!(
        !promoted.is_empty(),
        "no runtime entry point was promoted; imports: {:?}",
        program
            .imports
            .values()
            .map(|s| &s.symbol)
            .collect::<Vec<_>>()
    );
}
