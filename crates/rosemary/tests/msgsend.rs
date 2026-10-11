#![cfg(target_os = "macos")]

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

#[test]
fn an_objective_c_program_runs_with_builtin_hardening_enabled() {
    let Some(path) = common::objc_messages_fixture("objc_messages_debug", &[]) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let native = std::process::Command::new(&path)
        .output()
        .expect("run fixture natively");
    let output = common::driver_command()
        .args(["run", "--emulated", "--objc-debug", &path])
        .env("ROSEMARY_OBJC", "builtin")
        .output()
        .expect("run fixture with Objective-C hardening");
    assert_eq!(
        output.status.code(),
        native.status.code(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, native.stdout);
}

#[test]
fn the_same_program_optimised_takes_the_tail_call_path() {
    let Some(path) = common::objc_messages_fixture("objc_messages_o2", &["-O2"]) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let Some(_) = agrees(&path, &[]) else {
        skip!("cannot run x86_64 binaries natively")
    };
}

#[test]
fn every_objective_c_import_is_classified() {
    use rosemary::abi::Classification;

    let Some(path) = common::objc_messages_fixture("objc_classify", &[]) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let program = rosemary::Program::load(&path, &[]).expect("linking");
    let unknown: Vec<&str> = program
        .classify_imports()
        .into_iter()
        .filter(|(_, c)| *c == Classification::Unknown)
        .map(|(name, _)| name)
        .collect();
    assert!(unknown.is_empty(), "undescribed imports: {unknown:?}");
}

#[test]
fn the_message_send_family_is_never_promoted_to_a_fast_import() {
    use rosemary::abi::Classification;

    for symbol in ["_objc_msgSend", "_objc_msgSendSuper2", "_objc_alloc_init"] {
        assert_eq!(
            rosemary::abi::classify(0, symbol),
            Classification::Slow,
            "{symbol}"
        );
    }
}

#[test]
fn a_runtime_entry_point_that_cannot_dispatch_is_a_fast_import() {
    use rosemary::abi::Classification;

    for symbol in ["_class_getName", "_object_getClass", "_sel_registerName"] {
        assert_eq!(
            rosemary::abi::classify(0, symbol),
            Classification::Fast,
            "{symbol}"
        );
    }
}

#[test]
fn an_aggregate_return_and_a_variadic_method_both_work_and_throw_still_names_itself() {
    let Some(path) = common::objc_refusals_fixture() else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };

    // The same binary with no argument does none of the three and must run.
    let (status, stdout, stderr) = common::run_guest(&path, &[]);
    assert_eq!(status, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, "before\nafter\n");

    // `[s appendFormat:@"%d", 3]` - the format is rendered here and the method
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
        stderr.contains("libc++abi"),
        "throw: expected a refusal naming the C++ runtime, got {stderr:?}"
    );
}

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

#[test]
fn the_slow_import_policy_sends_the_same_messages() {
    let Some(path) = common::objc_messages_fixture("objc_messages_slow", &[]) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let (fast_status, fast, _) = common::run_guest(&path, &[]);
    let (slow_status, slow, stderr) = common::run_guest_with(&["--slow", "--emulated"], &path, &[]);
    assert_eq!(fast, slow, "stderr: {stderr}");
    assert_eq!(fast_status, slow_status);
}

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

#[test]
fn a_guest_method_whose_encoding_is_undecidable_is_still_sent() {
    let Some(path) = common::objc_guest_union_fixture() else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let Some((native_status, expected)) = common::run_native(&path) else {
        skip!("cannot run x86_64 binaries natively")
    };
    let (status, out, err) = common::run_guest(&path, &[]);

    assert_eq!(out, expected, "stderr: {err}");
    assert_eq!(status, native_status, "stderr: {err}");
    assert!(out.contains("1.50 7 9 2.25\n"), "{out}");
}

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

#[test]
fn a_linked_program_really_promotes_its_runtime_entry_points() {
    let Some(path) = common::objc_messages_fixture("objc_promote", &[]) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let program = rosemary::Program::load(&path, &[]).expect("linking");

    let mut promoted = Vec::new();
    for site in program.imports.values() {
        if rosemary::abi::classify(site.tag, &site.symbol) != rosemary::abi::Classification::Fast {
            continue;
        }
        let Some(p) = rosemary::objc::fast::promotion(&site.symbol, site.resolved) else {
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

#[test]
fn objective_c_exceptions_throw_and_catch_as_they_do_natively() {
    let Some(path) = common::objc_exceptions_fixture() else {
        skip!("cannot build x86_64 Objective-C++ fixtures")
    };

    let expected = "caught E r\nfinally\nsubclass N\nouter O\nid I\ndone\n";
    if let Some((status, stdout)) = common::run_native(&path) {
        assert_eq!(status, Some(0), "the fixture does not run on Apple's");
        assert_eq!(stdout, expected, "the fixture does not do this on Apple's");
    }

    let (status, stdout, stderr) =
        common::run_guest_env(&[("ROSEMARY_OBJC", "builtin")], &path, &[]);
    assert_eq!(status, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, expected);
}

#[test]
fn every_method_encoding_this_tree_installs_can_be_decoded() {
    foundation::bootstrap();
    appkit::bootstrap();

    let names: Vec<String> = foundation::CLASSES
        .iter()
        .map(|n| (*n).to_string())
        .chain(appkit::classes::names().into_iter().map(|n| n.to_string()))
        .collect();

    let mut checked = 0;
    for name in names {
        let Some(class) = std::ffi::CString::new(name.clone())
            .ok()
            .and_then(|n| objc::class::lookup(&n))
        else {
            continue;
        };
        for class in [class, class.metaclass()] {
            for method in class.own_methods() {
                // SAFETY: a `Method`'s types outlive the class, which is leaked.
                let types = unsafe { std::ffi::CStr::from_ptr(method.types()) }
                    .to_str()
                    .unwrap_or_else(|_| panic!("{name}: a type encoding that is not UTF-8"));
                // SAFETY: a registered selector's name is interned for the
                // life of the process.
                let selector = unsafe { std::ffi::CStr::from_ptr(objc::sel::name(method.sel)) }
                    .to_string_lossy()
                    .into_owned();
                let signature = rosemary::objc::encoding::signature(types).unwrap_or_else(|e| {
                    panic!("{name} {selector}: {types:?} does not decode: {e}")
                });
                let expected = 3 + selector.matches(':').count();
                assert_eq!(
                    signature.shapes.len(),
                    expected,
                    "{name} {selector}: {types:?} declares {} types, not {expected}",
                    signature.shapes.len(),
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 200, "only {checked} encodings were checked");
}

#[test]
fn an_argument_after_a_memory_aggregate_reaches_the_method() {
    let Some(path) = common::objc_after_memory_arg_fixture() else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let (status, stdout, stderr) = common::run_guest(&path, &[]);
    assert_eq!(status, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, "rect 1.5 2.5 30.5 40.5\noptions 1 owner 1\n");
}
