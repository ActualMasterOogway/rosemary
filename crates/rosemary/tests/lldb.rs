mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use common::{build_x86_64, driver, fault_fixture};

fn have_lldb() -> bool {
    let allowed = cfg!(not(target_os = "macos"))
        || std::env::var_os("ROSEMARY_ENABLE_PRIVILEGED_TESTS").is_some();
    allowed
        && Command::new("lldb")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
}

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
        .args(["--", driver(), "run", "--emulated", &path])
        .env("ROSEMARY_LLDB", "1")
        // lldb passes its environment to the driver it launches, so the
        // isolation has to be set here - see `common::test_prefix`.
        .env("ROSEMARY_PREFIX", common::test_prefix());

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

/// The other side of this is [`the_default_is_what_the_notice_says`], which is
/// the one that runs on a host whose LLDB constructs the plugin unasked.
#[test]
#[cfg(target_os = "macos")]
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
    // rosemary at all - a translation whose caller could not be walked is the
    // island this exists to rule out.
    // Taken from the *last* `frame #0:` in the output: the stop announcement
    // prints one and then the source listing around it, and `bt` prints the
    // frames again below that.
    //
    // "A frame below", not "the frame below": under a pinned link ABI the host
    // enters a region through an entry thunk (`microlift::core::pin`), and that
    // thunk is a real frame carrying no guest instruction and no symbol of its
    // own. So the walk out of the translation is one frame longer than it was,
    // and what has to hold is unchanged - a translation whose caller could not
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
            .any(|line| line.contains("rosemary`")),
        "the backtrace did not leave the translation into this runtime; {below:?}"
    );
}

#[test]
fn the_listing_is_the_x86_64_the_region_was_lifted_from() {
    let Some(path) = fault_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (_, out, err) = common::run_guest_env(&[("ROSEMARY_LLDB", "1")], &path, &[]);
    assert!(out.contains("about to fault"), "{out:?} (stderr: {err})");

    // The notice, which on macOS is the only thing that tells a user why LLDB is
    // silent. Elsewhere the plugin is on and there is nothing to say.
    assert_eq!(
        err.contains("plugin.jit-loader.gdb.enable on"),
        cfg!(target_os = "macos"),
        "stderr: {err}"
    );

    // The directory it named really has the listings in it.
    let directory = err
        .lines()
        .find_map(|line| line.strip_prefix("rosemary:   x86-64 listings: "))
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
    let mut guest = common::driver_command()
        .args(["run", "--emulated", &path])
        .env("ROSEMARY_LLDB", "1")
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

/// x86-64 that stores through a null pointer, and then returns. Real guest
/// instructions, because the listing is produced by decoding them.
const GUEST_X86_64: &[u8] = &[
    0x48, 0x31, 0xc0, // xor rax, rax
    0x48, 0x89, 0x00, // mov [rax], rax
    0xc3, // ret
];

/// The guest VA [`GUEST_X86_64`] is pretended to have been lifted from.
const GUEST_VA: u64 = 0x1_0000_2000;

/// The same thing in the host's own instruction set, which is what a lift of
/// the above would have produced and what actually runs.
#[cfg(target_arch = "aarch64")]
const HOST_CODE: &[u8] = &[
    0x00, 0x00, 0x80, 0xd2, // mov x0, #0
    0x00, 0x00, 0x00, 0xf9, // str x0, [x0]
    0xc0, 0x03, 0x5f, 0xd6, // ret
];

/// One range per guest instruction, as offsets into [`HOST_CODE`]: the shape of
/// the map a real lift produces.
#[cfg(target_arch = "aarch64")]
const HOST_BOUNDS: &[(u32, u32)] = &[(0, 4), (4, 8), (8, 12)];

/// The host being x86-64 too is a coincidence this test does not rely on: these
/// are the instructions that run, and [`GUEST_X86_64`] the ones the listing is
/// decoded from.
#[cfg(not(target_arch = "aarch64"))]
const HOST_CODE: &[u8] = &[
    0x48, 0x31, 0xc0, // xor rax, rax
    0x48, 0x89, 0x00, // mov [rax], rax
    0xc3, // ret
];

#[cfg(not(target_arch = "aarch64"))]
const HOST_BOUNDS: &[(u32, u32)] = &[(0, 3), (3, 6), (6, 7)];

/// How the faulting region is named: the shape [`rosemary::program::Program::locate`]
/// produces, so the symbol a debugger shows is the one a real stop would show.
const REGION: &str = "guest_fault+0x470";

#[test]
#[ignore = "the debuggee of lldb_symbolicates_a_published_region; faults on purpose"]
fn publish_a_region_and_fault() {
    use microlift::core::srcloc::{GuestCodeMap, GuestCodeRange};

    let len = HOST_CODE.len();
    // SAFETY: an anonymous mapping of one page, with no fixed address asked for.
    let code = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    assert_ne!(code, libc::MAP_FAILED, "mapping a page to execute from");

    // SAFETY: `code` is a page, and `HOST_CODE` is a handful of bytes of it.
    unsafe { std::ptr::copy_nonoverlapping(HOST_CODE.as_ptr(), code.cast(), len) };

    let map = GuestCodeMap::from_ranges(
        HOST_BOUNDS
            .iter()
            .zip(guest_instruction_starts())
            .map(|(&(start, end), guest_va)| GuestCodeRange {
                start,
                end,
                guest_va,
            })
            .collect(),
    );

    rosemary::lldb::register(
        code as u64,
        GUEST_VA,
        GUEST_VA + GUEST_X86_64.len() as u64,
        REGION,
        GUEST_X86_64,
        &map,
    );

    // SAFETY: the bytes just written are a function of no arguments that
    // returns - after it faults, which is the point.
    let entry: extern "C" fn() = unsafe { std::mem::transmute(code) };
    entry();
    panic!("the region was supposed to fault");
}

/// The guest VA of each instruction in [`GUEST_X86_64`].
fn guest_instruction_starts() -> [u64; 3] {
    [GUEST_VA, GUEST_VA + 3, GUEST_VA + 6]
}

fn lldb_at_the_published_region(settings: &[&str], commands: &[&str]) -> String {
    let exe = std::env::current_exe().expect("the path of this test binary");

    let mut command = Command::new("lldb");
    command.arg("--batch");
    for setting in settings {
        command.args(["-O", setting]);
    }
    command.args(["-o", "run"]);
    // `-k`, not `-o`, for the same reason as `lldb_at_the_fault`: the process
    // crashes, and the crash list is the one that runs then.
    for one in commands {
        command.args(["-k", one]);
    }
    let out = command
        .arg("--")
        .arg(&exe)
        .args([
            "publish_a_region_and_fault",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env("ROSEMARY_LLDB", "1")
        .output()
        .expect("running lldb");

    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    text
}

#[test]
fn lldb_symbolicates_a_published_region() {
    if !have_lldb() {
        skip!("no lldb on this host")
    }
    let text = lldb_at_the_published_region(&[ENABLE], &["bt"]);

    assert!(
        text.contains("stop reason = signal SIGSEGV") || text.contains("EXC_BAD_ACCESS"),
        "the debuggee did not fault under lldb; {text}"
    );
    assert!(
        !text.contains("failed to load module"),
        "LLDB refused the symfile; {text}"
    );

    // The same three facts the fixture-driven test asserts, about the same
    // frame: a module, a name and a line of the x86-64 listing. The name is the
    // one that catches the difference between the two formats - an ELF symbol's
    // value is an offset into its section and a Mach-O one's is an address, and
    // a frame whose symbol was placed at twice its address still has a module
    // and still has a line.
    let frame = text
        .lines()
        .find(|line| line.contains("frame #0:"))
        .unwrap_or_else(|| panic!("no frame #0 in lldb's output; {text}"));
    assert!(frame.contains("JIT(0x"), "not in a JIT module; {frame}");
    assert!(
        frame.contains("guest_fault+0x"),
        "not symbolicated against the region's name; {frame}"
    );
    assert!(
        frame.contains(".x86-64.s:"),
        "resolved to no line of the listing; {frame}"
    );
}

/// Whether LLDB reads the JIT interface without being asked, which is what the
/// notice `crate::lldb` prints on macOS (and prints nowhere else) claims.
#[test]
fn the_default_is_what_the_notice_says() {
    if !have_lldb() {
        skip!("no lldb on this host")
    }
    let text = lldb_at_the_published_region(&[LOG], &[]);

    assert_eq!(
        text.contains("registering JIT entry"),
        cfg!(not(target_os = "macos")),
        "the JIT loader's default changed on this platform, and the notice \
         `crate::lldb` prints has to change with it; {text}"
    );
}
