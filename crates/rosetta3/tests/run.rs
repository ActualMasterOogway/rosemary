//! Milestone 3's acceptance gate: Intel programs that actually run.
//!
//! Everything here goes through the built driver rather than the library, and
//! that is the point. The claims are about a *process* — it prints, it exits
//! with its own status, its `atexit` handlers fire from the host's exit — and
//! none of those are visible from inside a test that only calls
//! [`Vm::call`](rosetta3::Vm::call).
//!
//! Each test names what would be false if it failed:
//!
//! - **hello** — the loop works at all: lift, enter, service an import, resume.
//! - **qsort** — the host called *back into* the guest. Falsifiable: a
//!   comparator that is not really run leaves the array unsorted.
//! - **constructors** — initializers ran, in order, before `main`.
//! - **`/bin/ls`** — a real system binary agrees with the host's own, byte for
//!   byte, which is the claim no fixture can make on its own.

mod common;

use std::path::Path;

use common::{run_guest, run_guest_with};

#[test]
fn a_hello_world_prints_and_exits_zero() {
    let Some(path) = common::hello() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(
        out, "Hello, world (5)! 1 1.50\ndone\ngoodbye\n",
        "stderr: {err}"
    );
}

#[test]
fn a_guest_exit_status_reaches_the_shell() {
    // `main` returns `abs(argc) - 1`, so one argument makes it 1. A runtime
    // that always exited 0 would pass every other test here.
    let Some(path) = common::hello() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, _, err) = run_guest(&path, &["someone"]);
    assert_eq!(status, Some(1), "stderr: {err}");
}

#[test]
fn arguments_reach_the_guest() {
    let Some(path) = common::hello() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (_, out, err) = run_guest(&path, &["rosetta"]);
    assert!(
        out.starts_with("Hello, rosetta (7)! 2 1.50\n"),
        "{out:?} (stderr: {err})"
    );
}

#[test]
fn the_host_calls_a_guest_comparator_and_a_guest_exit_handler() {
    // The host→guest thunk, from both directions it is reached from: `qsort`
    // calls back during the call, `atexit`'s handler after `main` has returned
    // and from inside the host's own `exit`.
    let Some(path) = common::qsort_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, "12345789 calls=yes\nbye\n", "stderr: {err}");
}

#[test]
fn initializers_run_before_main_and_in_order() {
    let Some(path) = common::ctor_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, "order=abc\n", "stderr: {err}");
}

#[test]
fn a_destructor_runs_at_exit_in_one_list_with_the_guests_atexit_handler() {
    // `__cxa_atexit`, reached the way every real guest reaches it: the source
    // says `__attribute__((destructor))` and clang registers it from a
    // constructor. The order is the claim — the destructor is registered before
    // `main`, the `atexit` handler during it, and one reversed list prints them
    // in this order and no other.
    let Some(path) = common::dtor_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, "main\natexit\ndestructor\n", "stderr: {err}");
}

#[test]
fn fast_and_slow_imports_produce_the_same_program() {
    // Milestone 2 claimed promotion is an optimisation; this is the end-to-end
    // form of that claim. `--slow` sends every import through a VM exit and the
    // marshaller, `--fast` (the default) lets the lifter inline the ones it can.
    // Any observable difference means one of the two paths is wrong.
    let Some(path) = common::qsort_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };

    let (fast_status, fast_out, _) = run_guest(&path, &[]);
    let (slow_status, slow_out, slow_err) = run_guest_with(&["--slow"], &path, &[]);

    assert_eq!(fast_status, slow_status, "slow stderr: {slow_err}");
    assert_eq!(fast_out, slow_out);
}

#[test]
fn bin_ls_agrees_with_the_hosts_own_ls() {
    if !Path::new("/bin/ls").exists() {
        skip!("/bin/ls not present");
    }
    // A directory with entries of both kinds, and content that is stable while
    // the test runs — `ls` of anything shared would race with the rest of the
    // system.
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ls-fixture");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("subdir")).expect("create fixture dir");
    for name in ["alpha", "beta", "gamma"] {
        std::fs::write(dir.join(name), name).expect("write fixture file");
    }
    let dir = dir.to_string_lossy().into_owned();

    // `-l` as well as the bare form: the long listing pulls in `strftime`,
    // `getpwuid`, `snprintf` and the terminal width, so it exercises far more
    // of the import surface than the names alone.
    for flags in [&[][..], &["-1"][..], &["-la"][..]] {
        let mut args: Vec<&str> = flags.to_vec();
        args.push(&dir);

        let host = std::process::Command::new("/bin/ls")
            .args(&args)
            .output()
            .expect("running the host's ls");
        let (status, out, err) = run_guest("/bin/ls", &args);

        assert_eq!(status, host.status.code(), "ls {args:?}: stderr: {err}");
        assert_eq!(
            out,
            String::from_utf8_lossy(&host.stdout),
            "ls {args:?} differs from the host's own output"
        );
    }
}

#[test]
fn an_untranslatable_stop_names_itself_instead_of_crashing() {
    // The other half of "it runs": when it does not, the reason is a sentence.
    // `/usr/bin/true` is not a Mach-O this loader accepts on every system, so
    // the case chosen is one that is always available and always wrong — a
    // path that is not an executable at all.
    let (status, out, err) = run_guest("/etc/hosts", &[]);
    assert_eq!(status, Some(1));
    assert!(out.is_empty());
    assert!(err.starts_with("rosetta3: /etc/hosts:"), "{err:?}");
}

/// The `v`-prefixed formatting family, checked against the same binary run
/// natively.
///
/// The bug this exists to prevent was shipped and found by a real application:
/// a signature was written for `vfprintf` and no shim, so the *guest's*
/// `va_list` — SysV AMD64's two offsets, spill pointer and register save area —
/// was handed to the host's `vfprintf`, which reads arm64's, which is a bare
/// pointer. The result is not a wrong number, it is a fault inside `strlen`
/// several frames from anything that names the cause. Comparing whole output
/// against the native run is the only assertion worth making, for the same
/// reason it is in `printf.rs`: the specification of `vsnprintf` is what a C
/// library prints.
#[test]
fn the_va_list_family_prints_what_the_host_libc_prints() {
    let Some(path) = common::valist_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let Some((native_status, expected)) = common::run_native(&path) else {
        skip!("cannot run x86_64 binaries natively")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(out, expected, "stderr: {err}");
    assert_eq!(status, native_status, "stderr: {err}");
    // Named as well as diffed: the ninth argument of each class is the one in
    // the overflow area, and it is what a walker with the wrong step size gets
    // wrong first.
    assert!(out.contains("ints 1 2 3 4 5 6 7 8 9\n"), "{out}");
    assert!(
        out.contains("fps 1.5 2.5 3.5 4.5 5.5 6.5 7.5 8.5 9.5\n"),
        "{out}"
    );
}

/// The guest loads an Intel dylib of its own, mid-run, and calls into it.
///
/// The host's `dlopen` cannot answer this: it looks at the file, finds no arm64
/// slice, and refuses — correctly, to the wrong question. So the image is loaded
/// through `machish` into this process, and everything downstream of that has to
/// follow. What each line of the expected output would falsify:
///
/// - `plugin loaded` — the constructor ran, and it ran *before* `dlopen`
///   returned, which is what dyld guarantees and what a plugin depends on.
/// - `len=6` — the `dlsym` result is a **guest** address the runtime entered
///   through the JIT, and the `strlen` inside it reached the host's. That second
///   half is the one with no safety net: the plugin's call sites were never
///   registered with the lifter, so the call arrives as an `UnknownJump` and is
///   recognised at the exit or not at all. Not at all means decoding a `__stubs`
///   trampoline as guest code and disassembling native arm64 as x86-64.
/// - `missing=1 reported=1` — a name nothing defines is null *and* leaves a
///   message, rather than being answered by the host's `dlsym` with some
///   unrelated libSystem address.
#[test]
fn the_guest_loads_an_intel_dylib_of_its_own_and_calls_into_it() {
    let Some(path) = common::dlopen_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stdout: {out:?} stderr: {err}");
    assert_eq!(
        out, "plugin loaded\nlen=6\nmissing=1 reported=1\n",
        "stderr: {err}"
    );
}
