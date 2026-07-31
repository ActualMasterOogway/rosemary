//! `rosetta3 <path>` — load an Intel Mach-O, link it against the host, and lift
//! the first region a JIT would need.
//!
//! Milestone 1 stops before execution on purpose: nothing is run. What this
//! prints is the evidence that a runtime *could* run it — the linking
//! transcript, and one on-demand lift of the entry point's function, which is
//! exactly the unit of work the M3 execution loop will repeat.
//!
//! `--audit` additionally runs the whole-section call-target diagnostic. That is
//! a static analysis and deliberately *not* how the program would execute; see
//! [`rosetta3::audit`] for why a diagnostic is allowed to look at code the JIT
//! would never translate up front.

use std::collections::BTreeSet;
use std::process::ExitCode;

use rosetta3::{audit_call_targets, Program};

use microlift::arch::x86_64::X86_64;
use microlift::core::lift_to_function;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: rosetta3 <mach-o path> [--audit] [--ir]");
        return ExitCode::FAILURE;
    };
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

    // One lift, on demand, of the region control would actually reach first.
    // This is the execution model in miniature: a JIT repeats it, keyed on
    // wherever the previous region exited to.
    let config = program.lifter_config();
    match program.entry() {
        Some(entry) => match program.lift_region(entry) {
            Some((start, bytes)) => {
                println!("  entry {entry:#x} in function {start:#x} + {:#x}", bytes.len());
                match lift_to_function::<X86_64>(bytes, start, config) {
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
