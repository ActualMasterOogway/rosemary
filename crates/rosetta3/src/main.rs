//! `rosetta3 run [--slow] <path> [args...]` — run an Intel Mach-O on this
//! arm64 host.
//!
//! `rosetta3 <path>` without `run` inspects instead: it links the image, prints
//! the transcript, and performs one on-demand lift of the entry point's region
//! — the evidence that a runtime *could* run it, which is what milestone 1
//! stopped at and what remains the useful view when something does not work.
//!
//! `--audit` additionally runs the whole-section call-target diagnostic. That is
//! a static analysis and deliberately *not* how the program would execute; see
//! [`rosetta3::audit`] for why a diagnostic is allowed to look at code the JIT
//! would never translate up front.

use std::collections::BTreeSet;
use std::process::ExitCode;

use rosetta3::{audit_call_targets, startup, Classification, ImportPolicy, Program, Vm};

use microlift::arch::x86_64::X86_64;
use microlift::core::lift_to_function;

/// Load and execute, with `argv[0]` the path as the guest will see it.
fn run(path: &str, args: Vec<String>, policy: ImportPolicy) -> ExitCode {
    let mut argv = vec![path.to_string()];
    argv.extend(args);

    let program = match Program::load_executable(path, &[]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("rosetta3: {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let vm = match Vm::with_policy(program, path, &argv, policy) {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("rosetta3: {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Deliberately leaked, and published for the rest of the process. The
    // guest's `atexit` handlers are entries in the host's exit list and run
    // *after* this function returns; they re-enter the JIT, so the VM has to
    // still be there. Nothing is reclaimed at process exit in any case.
    let vm: &'static Vm = Box::leak(Box::new(vm));
    rosetta3::thunk::publish(vm);

    match startup::run(vm) {
        // `exit` rather than returning the status, and the difference is not
        // cosmetic. Returning goes back through Rust's own start-up shim, which
        // tears this thread's TLS down *before* calling `exit` — and the guest's
        // `atexit` handlers, which run from inside `exit`, re-enter the JIT.
        // Compiling a region needs live thread-locals (Cranelift's own, among
        // others), so a handler whose code had not yet been translated would
        // panic instead of running. Exiting from here keeps the runtime intact
        // for the handlers that are about to use it.
        Ok(status) => std::process::exit(status),
        // A stop is a diagnostic, not a crash: every `VmError` names the
        // address or the symbol that could not be handled, which is the whole
        // reason the loop refuses rather than guesses.
        Err(e) => {
            eprintln!("rosetta3: {path}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(first) = args.next() else {
        eprintln!("usage: rosetta3 run <mach-o path> [args...]");
        eprintln!("       rosetta3 <mach-o path> [--audit] [--ir]");
        return ExitCode::FAILURE;
    };
    if first == "run" {
        // `--slow` turns every import into a VM exit. It is here rather than
        // in a test harness because it is the falsifiable half of milestone
        // 2's claim: fast imports are an optimisation of slow ones, so the
        // same program run both ways must print the same thing.
        let mut policy = ImportPolicy::FastWhenKnown;
        let mut path = None;
        for arg in args.by_ref() {
            match arg.as_str() {
                "--slow" if path.is_none() => policy = ImportPolicy::AlwaysSlow,
                _ => {
                    path = Some(arg);
                    break;
                }
            }
        }
        let Some(path) = path else {
            eprintln!("usage: rosetta3 run [--slow] <mach-o path> [args...]");
            return ExitCode::FAILURE;
        };
        return run(&path, args.collect(), policy);
    }
    let path = first;
    let rest: Vec<String> = args.collect();
    let show_ir = rest.iter().any(|a| a == "--ir");
    let run_audit = rest.iter().any(|a| a == "--audit");

    let program = match Program::load(&path, &[]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("rosetta3: {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (text_va, text) = &program.text;
    println!("{path}");
    for img in &program.report.images {
        println!("  image {} at {:#x} (slide {:#x})", img.name, img.base, img.slide);
    }
    println!(
        "  __text {:#x} + {:#x}, {} function(s)",
        text_va,
        text.len(),
        program.functions.len()
    );
    println!(
        "  {} binding(s) -> {} registered address(es)",
        program.report.bindings().count(),
        program.imports.len()
    );
    let missing: usize = program.report.images.iter().map(|i| i.missing.len()).sum();
    if missing > 0 {
        println!("  {missing} import(s) unresolved (need shims)");
    }

    // What it would take to *call* each of those imports, which is milestone 2.
    // An unknown symbol is the only entry that matters: it is the one that
    // would become a wrong value in a register rather than a diagnostic.
    let classified = program.classify_imports();
    let count = |want: fn(&Classification) -> bool| classified.values().filter(|c| want(c)).count();
    println!(
        "  abi: {} fast, {} slow, {} refused, {} unknown",
        count(|c| matches!(c, Classification::Fast)),
        count(|c| matches!(c, Classification::Slow)),
        count(|c| matches!(c, Classification::Refused(_))),
        count(|c| matches!(c, Classification::Unknown)),
    );
    for (symbol, class) in &classified {
        match class {
            Classification::Unknown => println!("  abi: {symbol} has no signature"),
            Classification::Refused(reason) => println!("  abi: {symbol} refused: {reason}"),
            _ => {}
        }
    }

    // What the guest declares to the Objective-C runtime. Decoded and nothing
    // else — no class is realized, no selector uniqued, no category attached —
    // so this is a description of the work M6 has to do, printed as a count.
    for img in &program.report.images {
        let Some(objc) = &img.objc else { continue };
        let methods: usize = objc.implementations().count();
        println!(
            "  objc: {} class(es), {} categor(y/ies), {} protocol(s), {} method(s), {} selref(s)",
            objc.classes.len(),
            objc.categories.len(),
            objc.protocols.len(),
            methods,
            objc.selector_refs.len(),
        );
        for c in &objc.classes {
            println!(
                "  objc: class {} : {:#x}{}",
                c.ro.name,
                c.superclass,
                if c.ro.is_root() { " (root)" } else { "" }
            );
        }
        for c in &objc.categories {
            println!("  objc: category {} on {:#x}", c.name, c.class);
        }
        for r in &objc.refusals {
            println!("  objc: refused {r:?}");
        }
    }

    // One lift, on demand, of the region control would actually reach first.
    // This is the execution model in miniature: a JIT repeats it, keyed on
    // wherever the previous region exited to.
    let config = program.lifter_config();
    match program.entry() {
        Some(entry) => match program.lift_region(entry) {
            Some((region, bytes)) => {
                let start = region.range.start;
                println!("  entry {entry:#x} in region {start:#x} + {:#x}", bytes.len());
                match lift_to_function::<X86_64>(&bytes, start, config) {
                    Ok(func) => {
                        println!("  lifted to {} IR block(s)", func.dfg.num_blocks());
                        if show_ir {
                            println!("{}", func.display());
                        }
                    }
                    // An unimplemented instruction is a gap in the *recompiler*,
                    // not evidence that linking or interception is wrong, so it
                    // is reported rather than treated as a load failure.
                    Err(e) => println!("  lift failed: {e}"),
                }
            }
            None => println!("  entry {entry:#x} is outside any known function"),
        },
        None => println!("  no entry point (a dylib or bundle)"),
    }

    if !run_audit {
        return ExitCode::SUCCESS;
    }

    let registered: BTreeSet<u64> = program.import_addresses().collect();
    let audit =
        audit_call_targets(text, *text_va, &program.functions, program.text_range(), &registered);
    println!(
        "\n  audit: {} of {} function(s) analysable ({} refused by the lifter)",
        audit.functions_lifted,
        program.functions.len(),
        audit.functions_skipped
    );
    println!(
        "  audit: {} direct call target(s), {} leaving __text, {} recognised as imports",
        audit.direct_calls, audit.out_of_region, audit.intercepted
    );
    for u in &audit.unresolved {
        println!("  audit: call to {:#x} would be followed into as guest code", u.target);
    }

    if audit.is_clean() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
