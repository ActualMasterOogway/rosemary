#[allow(dead_code)]
mod sha256 {
    include!("src/sha256.rs");
}

use std::path::{Path, PathBuf};

/// The build directory of a `cargo build` inside the crate, which holds
/// products of the sources rather than sources.
const EXCLUDED: &[&str] = &["target"];

fn main() {
    let root = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );

    let mut files = Vec::new();
    collect(&root, &root, &mut files);
    // Hash in path order so the digest does not depend on directory order,
    // which no filesystem promises.
    files.sort();

    let mut hasher = sha256::Sha256::new();
    hasher.update_field(b"rosemary metal_translate crate content v1");
    hasher.update(&(files.len() as u64).to_le_bytes());
    for (relative, absolute) in &files {
        let contents = std::fs::read(absolute)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", absolute.display()));
        hasher.update_field(relative.as_bytes());
        hasher.update_field(&contents);
    }

    println!(
        "cargo::rustc-env=METAL_TRANSLATE_CACHE_VERSION={}",
        sha256::hex(&hasher.finish())
    );
}

/// Record every file under `directory`, and ask cargo to re-run for each.
///
/// Directories are declared as well as files: naming any path at all turns off
/// cargo's default of re-running whenever anything in the package changes, so
/// a file which is added or removed would otherwise not be noticed.
fn collect(root: &Path, directory: &Path, files: &mut Vec<(String, PathBuf)>) {
    println!("cargo::rerun-if-changed={}", directory.display());

    let entries = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
    for entry in entries {
        let entry = entry.expect("cannot read a directory entry");
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if EXCLUDED.contains(&relative.as_str()) {
            continue;
        }

        // Follow no symbolic links: a link into the workspace would make an
        // unrelated file part of this crate's version.
        let kind = entry
            .file_type()
            .unwrap_or_else(|error| panic!("cannot stat {}: {error}", path.display()));
        if kind.is_dir() {
            collect(root, &path, files);
        } else if kind.is_file() {
            println!("cargo::rerun-if-changed={}", path.display());
            files.push((relative, path));
        }
    }
}
