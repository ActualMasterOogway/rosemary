#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

pub fn system_binary(path: &str) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    is_mach_o(&bytes).then_some(bytes)
}

/// Whether `bytes` begins with a Mach-O or universal-binary magic.
pub fn is_mach_o(bytes: &[u8]) -> bool {
    let Some(magic) = bytes.get(..4) else {
        return false;
    };
    let magic = u32::from_be_bytes([magic[0], magic[1], magic[2], magic[3]]);
    matches!(
        magic,
        // Thin, 32- and 64-bit, either endianness...
        0xfeed_face | 0xcefa_edfe | 0xfeed_facf | 0xcffa_edfe
        // ...and fat, likewise.
        | 0xcafe_babe | 0xbeba_feca | 0xcafe_babf | 0xbfba_feca
    )
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

pub fn can_build_objc_x86_64() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| build_objc_x86_64("objc_probe", "int main(void){return 0;}").is_some())
}

pub fn build_objc_x86_64(name: &str, source: &str) -> Option<Vec<u8>> {
    build_objc_x86_64_flags(name, source, &[])
}

/// [`build_objc_x86_64`] with extra flags - `-Wl,-no_fixup_chains` above all,
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
