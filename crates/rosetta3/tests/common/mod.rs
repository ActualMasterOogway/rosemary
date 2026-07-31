//! Fixture plumbing for the driver's tests.
//!
//! Mirrors `machish/tests/common/mod.rs`, with one difference that matters:
//! [`Program::load`] takes a *path*, because `@executable_path` expansion is
//! part of linking, so these return where the fixture was written rather than
//! its bytes.
//!
//! Milestone 1's acceptance test uses `/bin/ls`. Milestone 2 uses a fixture we
//! compile, because the ABI question is about *which symbols a program calls*
//! and a locally built program lets the test state exactly that — and because
//! `/bin/ls` embeds a jump table in `__text` that two of its functions do not
//! lift past, which would put an unrelated microlift gap in the middle of an
//! ABI test.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

/// Where compiled fixtures are cached.
fn fixture_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fixtures");
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    dir
}

/// Whether this host can build x86_64 Mach-O binaries.
pub fn can_build_x86_64() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        let dir = fixture_dir();
        let src = dir.join("probe.c");
        if std::fs::write(&src, "int main(void){return 0;}").is_err() {
            return false;
        }
        Command::new("cc")
            .args(["-arch", "x86_64", "-o"])
            .arg(dir.join("probe"))
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Compile `source` into an x86_64 Mach-O and return its path.
pub fn build_x86_64(name: &str, source: &str, extra: &[&str]) -> Option<String> {
    if !can_build_x86_64() {
        return None;
    }
    let dir = fixture_dir();
    let src = dir.join(format!("{name}.c"));
    let out = dir.join(name);
    std::fs::write(&src, source).ok()?;

    let status = Command::new("cc")
        .args(["-arch", "x86_64"])
        .args(extra)
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .ok()?;
    status.success().then(|| out.to_string_lossy().into_owned())
}

/// The milestone 2 program: it prints, formats, and registers a callback, so
/// each of the three ways an import can be classified is exercised by a real
/// program rather than only by the table's own tests.
///
/// Deliberately *not* `int main(){puts("hi");}`:
///
/// - `printf`/`snprintf` are **variadic**, the part of the seam with no general
///   mechanism (and, at default `-D_FORTIFY_SOURCE`, they are imported under
///   their `___chk` names, which is what a real guest looks like).
/// - `atexit` takes a **guest function pointer**, so it must be refused until
///   the host→guest thunk exists in M3.
/// - `strlen`, `abs`, `puts` are the ordinary fast-callable case.
///
/// No `<math.h>`, and the `%f` argument is a constant: `sqrt` compiles to
/// `sqrtsd` and `argc * 1.5` to `cvtsi2sd`, neither of which microlift lifts
/// yet. A lifting gap inside an ABI fixture would be a red suite about the
/// wrong thing — the gap is real and it belongs to the recompiler.
pub const HELLO_C: &str = r#"
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

static void say_goodbye(void) { puts("goodbye"); }

int main(int argc, char **argv) {
    const char *who = argc > 1 ? argv[1] : "world";
    char buf[64];
    atexit(say_goodbye);
    snprintf(buf, sizeof buf, "%s (%zu)", who, strlen(who));
    printf("Hello, %s! %d %.2f\n", buf, argc, 1.5);
    puts("done");
    return abs(argc) - 1;
}
"#;

/// Build [`HELLO_C`], or `None` if this host cannot.
///
/// Built once per test binary: `cargo test` runs the tests in one binary
/// concurrently, and two of them compiling to the same path means one reads the
/// file while the other is writing it.
pub fn hello() -> Option<String> {
    static BUILT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| build_x86_64("hello", HELLO_C, &[])).clone()
}



/// Skip the calling test with an explanatory note.
#[macro_export]
macro_rules! skip {
    ($($arg:tt)*) => {{
        eprintln!("skipping {}: {}", module_path!(), format!($($arg)*));
        return;
    }};
}
