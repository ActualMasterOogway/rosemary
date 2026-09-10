//! Milestone 3's acceptance gate: Intel programs that actually run.
//!
//! Everything here goes through the built driver rather than the library, and
//! that is the point. The claims are about a *process* - it prints, it exits
//! with its own status, its `atexit` handlers fire from the host's exit - and
//! none of those are visible from inside a test that only calls
//! [`Vm::call`](rosetta3::Vm::call).
//!
//! Each test names what would be false if it failed:
//!
//! - **hello** - the loop works at all: lift, enter, service an import, resume.
//! - **qsort** - the host called *back into* the guest. Falsifiable: a
//!   comparator that is not really run leaves the array unsorted.
//! - **constructors** - initializers ran, in order, before `main`.
//! - **`/bin/ls`** - a real system binary agrees with the host's own, byte for
//!   byte, which is the claim no fixture can make on its own.

mod common;

use std::path::Path;
use std::process::Command;

use common::{run_guest, run_guest_with};

#[test]
#[cfg(target_os = "macos")]
fn objc_debug_rejects_the_native_objc_runtime_before_loading_the_guest() {
    let output = Command::new(common::driver())
        .args(["run", "--objc-debug", "/etc/hosts"])
        .env("ROSETTA3_OBJC", "native")
        .output()
        .expect("running the driver");
    assert_eq!(output.status.code(), Some(1));
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("--objc-debug requires"), "{err}");
    assert!(err.contains("ROSETTA3_OBJC=builtin"), "{err}");
}

#[test]
#[cfg(not(target_os = "macos"))]
fn objc_debug_is_accepted_with_the_default_builtin_runtime() {
    let output = Command::new(common::driver())
        .args(["run", "--objc-debug", "/etc/hosts"])
        .output()
        .expect("running the driver");
    assert_eq!(output.status.code(), Some(1));
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(!err.contains("usage:"), "{err}");
    assert!(!err.contains("--objc-debug requires"), "{err}");
    assert!(err.starts_with("rosetta3: /etc/hosts:"), "{err}");
}

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
    // The host->guest thunk, from both directions it is reached from: `qsort`
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
    // constructor. The order is the claim - the destructor is registered before
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
fn a_callback_is_translated_after_the_main_threads_tls_is_gone() {
    // macOS destroys the main thread's TLS inside `exit`, so an exit handler
    // that hands the host a callback it has never seen makes the runtime lift
    // and compile from a thread where every Rust `thread_local!` is already
    // gone. Cranelift's `timing` feature is one - hence the feature list in
    // `Cargo.toml`. Failure is exit status 70 and no `late=` line.
    let Some(path) = common::late_callback_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest(&path, &[]);

    assert_eq!(status, Some(0), "stderr: {err}");
    assert_eq!(out, "main\nlate=12345\n", "stderr: {err}");
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
    let (slow_status, slow_out, slow_err) = run_guest_with(&["--slow", "--emulated"], &path, &[]);

    assert_eq!(fast_status, slow_status, "slow stderr: {slow_err}");
    assert_eq!(fast_out, slow_out);
}

#[test]
fn bin_ls_agrees_with_the_hosts_own_ls() {
    if !common::host_macho("/bin/ls") {
        skip!("no host Mach-O at /bin/ls");
    }
    // A directory with entries of both kinds, and content that is stable while
    // the test runs - `ls` of anything shared would race with the rest of the
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
    // the case chosen is one that is always available and always wrong - a
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
/// `va_list` - SysV AMD64's two offsets, spill pointer and register save area -
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
/// slice, and refuses - correctly, to the wrong question. So the image is loaded
/// through `machish` into this process, and everything downstream of that has to
/// follow. What each line of the expected output would falsify:
///
/// - `plugin loaded 42` - the constructor ran before `dlopen` returned and its
///   flat import resolved back into the main executable.
/// - `len=6` - the `dlsym` result is a **guest** address the runtime entered
///   through the JIT, and the `strlen` inside it reached the host's. That second
///   half is the one with no safety net: the plugin did not exist when the
///   lifter was configured, so its call sites are recognised by
///   [`rosetta3::bridge::DynamicImports`] while the plugin is being lifted, or
///   at the exit, or not at all. Not at all means decoding a `__stubs`
///   trampoline as guest code and disassembling native arm64 as x86-64.
/// - `missing=1 reported=1` - a name nothing defines is null *and* leaves a
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
        out, "plugin loaded 42\nlen=6\nmissing=1 reported=1\n",
        "stderr: {err}"
    );
}

/// The native backend uses the same guest dyld implementation, but has no VM
/// to run a loaded image's constructor. The active [`rosetta3::Execution`]
/// must dispatch that call through [`rosetta3::Native`] instead.
#[test]
fn the_native_guest_loads_a_dylib_and_runs_its_constructor() {
    if !cfg!(target_arch = "x86_64") {
        skip!("the x86-64 fixture is not native on this host")
    }
    let Some(path) = common::dlopen_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };
    let (status, out, err) = run_guest_with(&["--native"], &path, &[]);

    assert_eq!(status, Some(0), "stdout: {out:?} stderr: {err}");
    assert_eq!(
        out, "plugin loaded 42\nlen=6\nmissing=1 reported=1\n",
        "stderr: {err}"
    );
}

#[test]
fn a_dynamically_loaded_image_runs_the_same_bridged_and_exited() {
    // The falsifiable half of the lift-time import registry, and the reason it
    // is worth stating separately from the fixture's own expectations.
    //
    // Under `--slow` there is no registry and no bridge: the plugin's call sites
    // are unknown to the lifter, so each one leaves microlift as an
    // `UnknownJump` and is recognised by the run loop's own lookup before it
    // enters the region. Under the default the same sites are recognised *while
    // the plugin is lifted* and become inline calls to
    // [`rosetta3::bridge::import_call`].
    //
    // Two entirely different routes to the same host function, which is exactly
    // the shape a silent divergence would hide in - a stale register reloaded
    // around the bridge, a return address pushed twice, a `dlsym`ed pointer
    // dispatched as host code on one path and guest code on the other. The
    // program is small enough that any of those changes what it prints.
    let Some(path) = common::dlopen_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };

    let (fast_status, fast_out, fast_err) = run_guest(&path, &[]);
    let (slow_status, slow_out, slow_err) = run_guest_with(&["--slow", "--emulated"], &path, &[]);

    assert_eq!(fast_status, Some(0), "stderr: {fast_err}");
    assert_eq!(slow_status, Some(0), "stderr: {slow_err}");
    assert_eq!(fast_out, slow_out);
}

/// The mode a guest passes to `open` is the mode the file gets - through the
/// inlined call *and* through the bridged one.
///
/// `open`, `openat` and `sem_open` are described to the lifter as fixed-arity,
/// with the arguments their ellipsis may carry always present, and are bound to
/// wrappers of this runtime's that apply the `O_CREAT` rule themselves. See
/// [`rosetta3::shims::openat`]. The permission bits on disk are what says the
/// mode arrived where the signature claims: they are odd numbers no umask and no
/// default produces, so a mode read from the wrong register - or from the stack,
/// which is where a genuinely variadic callee would look - gives a file that
/// fails this outright. The semaphore's count is the same claim for the pair of
/// optional arguments.
///
/// `--slow` runs it a second time because the two routes place the same
/// argument by different code: an absolute native call the lifter baked in, and
/// the marshaller at a VM exit.
#[test]
fn the_mode_a_guest_opens_with_is_the_mode_the_file_gets() {
    let Some(path) = common::open_fixture() else {
        skip!("cannot build x86_64 fixtures")
    };

    for extra in [&["--emulated"][..], &["--slow", "--emulated"][..]] {
        let dir = std::env::temp_dir().join(format!("rosetta3-open-mode{}", extra.len()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory the guest can write to");

        let (status, out, err) = run_guest_with(extra, &path, &[dir.to_str().unwrap()]);
        assert_eq!(status, Some(0), "stdout: {out:?} stderr: {err}");
        // `sem 110`: the semaphore was created holding exactly the two the
        // guest asked for, which is the pair of optional arguments a variadic
        // callee would have looked for on the stack.
        assert_eq!(
            out, "open ok\nreopen ok\nopenat ok\nsem 110\n",
            "stderr: {err}"
        );

        assert_eq!(mode_of(&dir.join("created")), 0o604, "open's mode");
        assert_eq!(mode_of(&dir.join("created-at")), 0o641, "openat's mode");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A file's permission bits, as the guest's `umask(0)` left them.
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o7777
}
