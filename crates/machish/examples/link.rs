//! Link a Mach-O and print the transcript.
//!
//! ```sh
//! cargo run -p machish --example link -- /bin/ls
//! ```
//!
//! This is the crate's own diagnostic, and it exists because
//! [`LinkReport`](machish::report::LinkReport) is the product — not a debug
//! aid. An embedder consumes exactly what this prints: for each binding, the
//! slot address, the stub address, the library, the symbol, and what it
//! resolved to. If the interception the driver installs is wrong, the answer is
//! visible here first.

use std::process::ExitCode;

use machish::install_name::is_system_path;
use machish::loader::{LoadPolicy, Loader};
use machish::memory::BufferSpace;
use machish::namespace::FsDylibSource;
use machish::report::LinkReport;
use machish::resolve::{NativeDylibProvider, SymbolProvider, eager_binding_shims};
use machish::spec::arch::X86_64;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: link <mach-o path> [--strict] [--quiet]");
        return ExitCode::FAILURE;
    };
    let rest: Vec<String> = args.collect();
    let strict = rest.iter().any(|a| a == "--strict");
    let quiet = rest.iter().any(|a| a == "--quiet");

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("link: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The default host configuration: bundled dylibs from disk, `/usr/lib`
    // declined and answered natively.
    let mut mem = BufferSpace::new(0x1_0000_0000);
    let mut source = FsDylibSource::new();
    let shims = eager_binding_shims();
    let native = NativeDylibProvider::system();
    let providers: [&dyn SymbolProvider; 2] = [&shims, &native];

    let policy = LoadPolicy {
        require_all_symbols: strict,
        ..LoadPolicy::default()
    };
    let report = match Loader::with_policy(X86_64, policy).link_with(
        &bytes,
        &path,
        &mut mem,
        &mut source,
        &providers,
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("link: {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    print(&report, quiet);
    if report
        .images
        .iter()
        .any(|i| i.hard_missing().next().is_some())
    {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn print(report: &LinkReport, quiet: bool) {
    for img in &report.images {
        println!(
            "image {}\n  base {:#x}  slide {:#x}  fixups {}  rebases {}",
            img.name, img.base, img.slide, img.encoding, img.rebases
        );
        if let Some(entry) = img.entry {
            println!("  entry {entry:#x}");
        }
        for seg in &img.segments {
            let file = match &seg.file_range {
                Some(r) => format!("file [{:#x}, {:#x})", r.start, r.end),
                None => "zero-fill".to_string(),
            };
            println!(
                "  segment {:<12} {:#014x} + {:#x}  {}{}",
                seg.name,
                seg.vmaddr,
                seg.vmsize,
                file,
                if seg.executable { "  code" } else { "" }
            );
        }

        if !quiet {
            for b in &img.bindings {
                // The stub column is the load-bearing one: it is the address a
                // `call rel32` actually targets, and the only handle an
                // interception has on the common call shape.
                let stub = match b.stub_va {
                    Some(s) => format!("{s:#012x}"),
                    None => "     -      ".to_string(),
                };
                let library = b.library.as_deref().unwrap_or("<flat>");
                let via = if is_system_path(library) {
                    "native"
                } else {
                    "image"
                };
                println!(
                    "  bind {:#012x} stub {} {:<40} {:<32} -> {:#x} [{via}]",
                    b.slot_va,
                    stub,
                    short(library),
                    b.symbol,
                    b.resolved
                );
            }
        }

        for init in &img.initializers {
            println!("  initializer {init:#x}  (collected, not run)");
        }
        for tlv in &img.thread_locals {
            println!(
                "  thread-local {:#x}  thunk {:#x} key {} offset {:#x}",
                tlv.at, tlv.thunk, tlv.key, tlv.offset
            );
        }
        if let Some(objc) = &img.objc {
            if let Some(info) = objc.image_info {
                println!(
                    "  objc image info version {} flags {:#x}",
                    info.version, info.flags
                );
            }
            for c in &objc.classes {
                let meta_methods = c.meta.as_ref().map_or(0, |m| m.ro.methods.len());
                println!(
                    "  objc class {:#x} {}  super {:#x}  {} method(s), {} class method(s), {} ivar(s), {} protocol(s)",
                    c.va,
                    c.ro.name,
                    c.superclass,
                    c.ro.methods.len(),
                    meta_methods,
                    c.ro.ivars.len(),
                    c.ro.protocols.len()
                );
                for m in
                    c.ro.methods
                        .iter()
                        .chain(c.meta.iter().flat_map(|m| &m.ro.methods))
                {
                    println!(
                        "    -[{} {}] {} imp {:#x}",
                        c.ro.name, m.name, m.types, m.imp
                    );
                }
            }
            for c in &objc.categories {
                println!(
                    "  objc category {:#x} ({})  on class {:#x}  {} method(s)",
                    c.va,
                    c.name,
                    c.class,
                    c.instance_methods.len() + c.class_methods.len()
                );
            }
            for p in &objc.protocols {
                println!(
                    "  objc protocol {:#x} {}  {} required, {} optional",
                    p.va,
                    p.name,
                    p.instance_methods.len() + p.class_methods.len(),
                    p.optional_instance_methods.len() + p.optional_class_methods.len()
                );
            }
            println!(
                "  objc {} selref(s), {} classref(s), {} superref(s)",
                objc.selector_refs.len(),
                objc.class_refs.len(),
                objc.super_refs.len()
            );
            for r in &objc.refusals {
                println!("  objc REFUSED {r:?}");
            }
        }

        for m in &img.missing {
            println!(
                "  MISSING {} from {} at {:#x}{}",
                m.symbol,
                m.library.as_deref().unwrap_or("<flat>"),
                m.slot_va,
                if m.weak_import {
                    " (weak, bound to zero)"
                } else {
                    ""
                }
            );
        }
        println!();
    }

    let total = report.bindings().count();
    let with_stub = report.bindings().filter(|b| b.stub_va.is_some()).count();
    let missing: usize = report.images.iter().map(|i| i.missing.len()).sum();
    println!(
        "{} image(s), {total} binding(s), {with_stub} with a stub, {missing} unresolved",
        report.images.len()
    );
}

/// Trim a long install name to its last two components, so the table stays
/// readable without losing which library a symbol came from.
fn short(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').take(2).collect();
    if parts.len() < 2 {
        return path.to_string();
    }
    format!("{}/{}", parts[1], parts[0])
}
