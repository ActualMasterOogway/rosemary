//! Translated code is visible to a real LLDB.
//!
//! Every one of these drives the `lldb` on this machine, because the previous
//! implementation of this feature passed every test that did not: it built a
//! Mach-O, it filled in DWARF, it walked the JIT ABI correctly — and LLDB
//! showed nothing whatsoever, for two reasons neither of which is observable
//! from inside the process. The plugin that reads the JIT interface is off by
//! default on macOS, and the object named its code with an absolute symbol,
//! which LLDB loads at no address at all.
//!
//! So the assertions here are about what a debugger *says*, not about what this
//! crate emitted. What each would falsify:
//!
//! - **the plugin's macOS default is still what it is** — the test that turns
//!   it on and the one that leaves it off, together, are the reason the notice
//!   `crate::lldb` prints exists; if Apple ever flips the default, the second
//!   test fails and the notice can go;
//! - **LLDB parses the symfile** — anything malformed shows up as "failed to
//!   load module for JIT entry", which is what the object crate's output
//!   *avoided* while still being useless;
//! - **the code gets a load address**, which is the bug that made the previous
//!   version report nothing;
//! - **a frame in translated code is symbolicated** — the entire point: a stop
//!   inside a lift names the guest function and a line of x86-64, instead of a
//!   bare `pc` in an anonymous mapping.

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use common::{build_x86_64, driver, fault_fixture};

/// Whether this host has an `lldb` that can debug what we just built.
fn have_lldb() -> bool {
    Command::new("lldb")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Run the driver under `lldb` on the faulting fixture, with `commands` issued
/// after the process stops, and return everything LLDB printed.
///
/// The fault fixture earns its place here: it stops the process *inside*
/// translated code, at an address LLDB has to resolve through what this module
/// published, without needing a breakpoint on generated code that no test could
/// name ahead of time.
fn lldb_at_the_fault(settings: &[&str], commands: &[&str]) -> Option<String> {
    let path = fault_fixture()?;

    let mut command = Command::new("lldb");
    command.arg("--batch");
    for setting in settings {
        command.args(["-O", setting]);
    }
    command.args(["-o", "run"]);
    // `-k`, not `-o`: the process crashes, and `--batch` abandons the rest of
    // its `-o` list when a stop was not the one it asked for. The crash list is
    // the one that runs here.
    for one in commands {
        command.args(["-k", one]);
    }
    command
        .args(["--", driver(), "run", &path])
        .env("ROSETTA3_LLDB", "1");

    let out = command.output().ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(text)
}

const ENABLE: &str = "settings set plugin.jit-loader.gdb.enable on";
const LOG: &str = "log enable lldb jit";

#[test]
fn lldb_loads_every_translation_it_is_handed() {
    if !have_lldb() {
        skip!("no lldb on this host")
    }
    let Some(text) = lldb_at_the_fault(&[ENABLE, LOG], &[]) else {
        skip!("cannot build x86_64 fixtures")
    };

    // It got as far as the fault, so translations really were compiled.
    assert!(text.contains("about to fault"), "{text}");
    // And LLDB was told about them.
    assert!(
        text.contains("registering JIT entry at 0x"),
        "LLDB never saw a registration; {text}"
    );
    // The assertion the previous implementation would also have passed, kept
    // because it is the one that catches a malformed Mach-O.
    assert!(
        !text.contains("failed to load module"),
        "LLDB refused the symfile; {text}"
    );
}

#[test]
fn the_macos_default_publishes_to_nobody() {
    if !have_lldb() {
        skip!("no lldb on this host")
    }
    // Same run, without the setting. This is the whole reason the feature
    // looked broken: not one registration is read, however correct the rest is.
    let Some(text) = lldb_at_the_fault(&[LOG], &[]) else {
        skip!("cannot build x86_64 fixtures")
    };

    assert!(text.contains("about to fault"), "{text}");
    assert!(
        !text.contains("registering JIT entry"),
        "the JIT loader is enabled by default now, and `crate::lldb`'s notice \
         (and this test) can go; {text}"
    );
}

#[test]
fn a_frame_in_translated_code_names_the_guest_it_came_from() {
    if !have_lldb() {
        skip!("no lldb on this host")
    }
    let Some(text) = lldb_at_the_fault(&[ENABLE], &["bt"]) else {
        skip!("cannot build x86_64 fixtures")
    };

    // The process stopped where it was meant to: in the guest's own bad store,
    // not somewhere in the runtime.
    assert!(
        text.contains("stop reason = signal SIGSEGV") || text.contains("EXC_BAD_ACCESS"),
        "the fixture did not fault under lldb; {text}"
    );

    // One line, all three facts, because each of them alone can be true for the
    // wrong reason: `crate::fault` prints its own `guest_fault+0x...` to the
    // same stream, and a JIT module can exist with nothing resolving through
    // it. What is asserted is a *frame* that has a module, a name and a line.
    let frame = text
        .lines()
        .find(|line| line.contains("frame #0:"))
        .unwrap_or_else(|| panic!("no frame #0 in lldb's output; {text}"));

    assert!(frame.contains("JIT(0x"), "not in a JIT module; {frame}");
    assert!(
        frame.contains("guest_fault+0x"),
        "not symbolicated against the guest image; {frame}"
    );
    assert!(
        frame.contains(".x86-64.s:"),
        "resolved to no line of the listing; {frame}"
    );

    // And the frame below it is this runtime's own, which is what makes the
    // stop a place to work from rather than an island.
    //
    // Asserted as *the next frame in this module*, rather than by naming the
    // function or the file it is in: which of `Translation::enter` and
    // `Vm::run_loop` the caller resolves to is a matter of what the optimiser
    // inlined into what, and it changed when the entry became a dispatch loop
    // (see `Translation::enter`). What has to hold is that frame #1 is in
    // rosetta3 at all — a translation whose caller could not be walked is the
    // island this exists to rule out.
    // Taken from the *last* `frame #0:` in the output: the stop announcement
    // prints one and then the source listing around it, and `bt` prints the
    // frames again below that.
    //
    // "A frame below", not "the frame below": under a pinned link ABI the host
    // enters a region through an entry thunk (`microlift::core::pin`), and that
    // thunk is a real frame carrying no guest instruction and no symbol of its
    // own. So the walk out of the translation is one frame longer than it was,
    // and what has to hold is unchanged — a translation whose caller could not
    // be walked is the island this exists to rule out.
    let below: Vec<&str> = text
        .lines()
        .collect::<Vec<_>>()
        .rsplit(|line| line.contains("frame #0:"))
        .next()
        .map(|after| after.to_vec())
        .unwrap_or_default();
    assert!(
        below
            .iter()
            .take_while(|line| line.contains("frame #"))
            .any(|line| line.contains("rosetta3`")),
        "the backtrace did not leave the translation into this runtime; {below:?}"
    );
}

#[test]
fn the_listing_is_the_x86_64_the_region_was_lifted_from() {
    let Some(path) = fault_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (_, out, err) = common::run_guest_env(&[("ROSETTA3_LLDB", "1")], &path, &[]);
    assert!(out.contains("about to fault"), "{out:?} (stderr: {err})");

    // The notice, which is the only thing that tells a user why LLDB is silent.
    assert!(
        err.contains("plugin.jit-loader.gdb.enable on"),
        "stderr: {err}"
    );

    // The directory it named really has the listings in it.
    let directory = err
        .lines()
        .find_map(|line| line.strip_prefix("rosetta3:   x86-64 listings: "))
        .expect("the notice says where listings go");
    let listings: Vec<_> = std::fs::read_dir(directory)
        .expect("listing directory")
        .filter_map(|entry| entry.ok())
        .collect();
    assert!(!listings.is_empty(), "no listings in {directory}");

    // One file per guest image, not one per region: a program lifts thousands.
    let fixture = listings
        .iter()
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("guest_fault")
        })
        .expect("a listing for the fixture's own image");
    let text = std::fs::read_to_string(fixture.path()).expect("read the listing");

    // Guest addresses and x86-64 mnemonics, which is what a line of it has to
    // be for a DWARF row pointing at it to mean anything.
    assert!(text.contains("// ---- guest_fault+0x"), "{text:.4000}");
    assert!(
        text.lines().filter(|line| line.starts_with("0x")).count() > 16,
        "barely any instructions listed; {text:.4000}"
    );
    // The fixture's own faulting store, which `crate::fault` independently
    // names as `mov qword ptr [rax], 1`.
    assert!(text.contains("mov "), "{text:.4000}");

    std::fs::remove_dir_all(directory).ok();
}

/// A guest with a lot of functions in it and a long tail, so a debugger can
/// arrive after everything has already been compiled.
///
/// Thirty of them, and mutually recursive so none is folded away: the point is
/// to have many *regions* registered before the attach, because a chain of one
/// would pass whether or not the list is really a list.
fn many_regions() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let mut source = String::from(
                "#include <stdio.h>\n#include <unistd.h>\n\
                 __attribute__((noinline)) int f0(int x) { return x; }\n",
            );
            for i in 1..30 {
                source.push_str(&format!(
                    "__attribute__((noinline)) int f{i}(int x) \
                     {{ return x * {i} + f{} (x - 1) % 7; }}\n",
                    i - 1
                ));
            }
            source.push_str(
                "int main(void) {\n\
                     int sum = 0;\n\
                     for (int i = 0; i < 40; i++) sum += f29(i);\n\
                     printf(\"compiled %d\\n\", sum);\n\
                     fflush(stdout);\n\
                     for (int i = 0; i < 30; i++) sleep(1);\n\
                     return 0;\n\
                 }\n",
            );
            build_x86_64("many_regions", &source, &["-O0"])
        })
        .clone()
}

#[test]
fn a_debugger_that_arrives_late_still_sees_everything() {
    if !have_lldb() {
        skip!("no lldb on this host")
    }
    let Some(path) = many_regions() else {
        skip!("cannot build x86_64 fixtures")
    };

    // Entries are prepended to a list that is never unlinked, precisely so this
    // works: on attach LLDB walks from `first_entry` and registers the lot. A
    // version that only ever published the newest translation would pass every
    // other test in this file and fail this one.
    let mut guest = Command::new(driver())
        .args(["run", &path])
        .env("ROSETTA3_LLDB", "1")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawning the driver");

    // Wait for the compiling to be over rather than sleeping and hoping.
    let mut stdout = BufReader::new(guest.stdout.take().expect("piped stdout"));
    let mut line = String::new();
    let compiled = stdout.read_line(&mut line).is_ok() && line.starts_with("compiled");

    let text = if compiled {
        let out = Command::new("lldb")
            .arg("--batch")
            .args(["-O", ENABLE])
            .args(["-O", LOG])
            .args(["-p", &guest.id().to_string()])
            .args(["-o", "detach"])
            .output()
            .expect("attaching lldb");
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        text
    } else {
        String::new()
    };

    guest.kill().ok();
    guest.wait().ok();

    if !compiled {
        skip!("the fixture never reported that it had run")
    }
    if text.contains("attach failed") || text.contains("Operation not permitted") {
        skip!("this host will not let a test attach to its own child")
    }

    let registered = text.matches("registering JIT entry").count();
    assert!(
        registered > 20,
        "only {registered} translation(s) came back on attach, out of the thirty \
         functions the fixture calls; {text}"
    );
    assert!(
        !text.contains("failed to load module"),
        "LLDB refused a symfile it read from the list; {text}"
    );
}
