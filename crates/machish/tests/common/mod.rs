//! Shared fixture plumbing.
//!
//! Two sources of real x86_64 Mach-O bytes, mirroring `selfish`'s
//! build-or-fall-back pattern (`selfish/tests/aarch64_shim.rs:13-31`):
//!
//! - **System binaries.** `/bin/ls` and friends are universal binaries with
//!   genuine x86_64 slices, free and representative. They are read at test time
//!   and the test is *skipped* rather than failed if absent, since a machine
//!   without them is a legitimate configuration.
//! - **Locally built.** `cc -arch x86_64` works on an Apple Silicon host, so
//!   fixtures are compiled on demand into the target directory.
//!
//! Assertions on system binaries must be **structural**, never on literal
//! addresses: a macOS update relays out `/bin/ls` and would turn a green suite
//! red for no reason.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

/// Read a system binary, or `None` if it is not present.
pub fn system_binary(path: &str) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

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

/// Compile `source` into an x86_64 Mach-O and return its bytes.
///
/// `extra` supplies additional linker/compiler flags — notably
/// `-Wl,-no_fixup_chains`, which is how a modern toolchain is made to emit
/// `LC_DYLD_INFO_ONLY` instead of chained fixups.
pub fn build_x86_64(name: &str, source: &str, extra: &[&str]) -> Option<Vec<u8>> {
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
    if !status.success() {
        return None;
    }
    std::fs::read(&out).ok()
}

/// Whether this host can build x86_64 Objective-C binaries.
///
/// A separate probe from [`can_build_x86_64`], because it asks a different
/// question: the x86_64 *SDK* has the Foundation headers and the x86_64 slice
/// of the framework stub, and a machine with the compiler but not the SDK is a
/// configuration a test must skip on rather than fail.
pub fn can_build_objc_x86_64() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| build_objc_x86_64("objc_probe", "int main(void){return 0;}").is_some())
}

/// Compile an Objective-C `source` into an x86_64 Mach-O and return its bytes.
///
/// `-framework Foundation` is not optional even for a fixture that only
/// declares a root class: without it there is no `NSObject` to derive from, and
/// a class with no superclass exercises a code path real programs never take.
pub fn build_objc_x86_64(name: &str, source: &str) -> Option<Vec<u8>> {
    build_objc_x86_64_flags(name, source, &[])
}

/// [`build_objc_x86_64`] with extra flags — `-Wl,-no_fixup_chains` above all,
/// which is how a modern toolchain is made to emit `LC_DYLD_INFO_ONLY`.
pub fn build_objc_x86_64_flags(name: &str, source: &str, extra: &[&str]) -> Option<Vec<u8>> {
    if !can_build_x86_64() {
        return None;
    }
    let dir = fixture_dir();
    let src = dir.join(format!("{name}.m"));
    let out = dir.join(name);
    std::fs::write(&src, source).ok()?;

    let status = Command::new("cc")
        .args(["-arch", "x86_64", "-framework", "Foundation"])
        .args(extra)
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    std::fs::read(&out).ok()
}

/// Skip the calling test with an explanatory note.
#[macro_export]
macro_rules! skip {
    ($($arg:tt)*) => {{
        eprintln!("skipping {}: {}", module_path!(), format!($($arg)*));
        return;
    }};
}
