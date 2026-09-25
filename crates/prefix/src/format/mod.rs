pub mod manifest;
pub mod spec;
pub mod yaml;

use std::fmt;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

pub use manifest::Manifest;
pub use spec::Spec;

/// The only format version defined.
pub const FORMAT: i64 = 1;

/// A spec is small; refuse to buffer anything that is not.
pub const SPEC_LIMIT: u64 = 1 << 20;
pub const MANIFEST_LIMIT: u64 = 64 << 20;

/// Input that is not what it claims to be, or a check that did not pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid(pub String);

impl Invalid {
    pub fn new(message: impl Into<String>) -> Invalid {
        Invalid(message.into())
    }

    pub(crate) fn at(what: &str, line: usize, message: &str) -> Invalid {
        Invalid(format!("{what}:{line}: {message}"))
    }
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Invalid {}

impl From<std::io::Error> for Invalid {
    fn from(error: std::io::Error) -> Invalid {
        Invalid(error.to_string())
    }
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The SHA-256 and length of everything `reader` yields.
pub fn sha256_reader(mut reader: impl Read) -> std::io::Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buffer[..n]);
        size += n as u64;
    }
    Ok((hex(&hasher.finalize()), size))
}

pub fn sha256_file(path: &Path) -> std::io::Result<(String, u64)> {
    sha256_reader(std::fs::File::open(path)?)
}

/// Whether `value` is 64 lowercase hex digits.
pub fn is_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Read at most `limit` bytes of a small text file, refusing a larger one.
pub fn read_limited(path: &Path, limit: u64, what: &str) -> Result<Vec<u8>, Invalid> {
    let file = std::fs::File::open(path).map_err(|e| Invalid(format!("{what}: {e}")))?;
    let mut raw = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut raw)
        .map_err(|e| Invalid(format!("{what}: {e}")))?;
    if raw.len() as u64 > limit {
        return Err(Invalid(format!("{what}: larger than {limit} bytes")));
    }
    Ok(raw)
}

/// Printable ASCII without `/`. No Unicode in format 1: normalisation is
/// exactly where two filesystems disagree about whether two names are one.
pub fn is_name(part: &str) -> bool {
    !part.is_empty()
        && part
            .bytes()
            .all(|b| (0x20..=0x7e).contains(&b) && b != b'/')
}

/// `/usr/lib/x` as `["usr", "lib", "x"]`, refusing anything that is not a
/// plain absolute path with safe components.
pub fn split_path<'a>(path: &'a str, what: &str) -> Result<Vec<&'a str>, Invalid> {
    let Some(rest) = path.strip_prefix('/').filter(|rest| !rest.is_empty()) else {
        return Err(Invalid(format!(
            "{what}: '{path}' is not an absolute path below /"
        )));
    };
    let parts: Vec<&str> = rest.split('/').collect();
    for part in &parts {
        if matches!(*part, "" | "." | "..") || !is_name(part) {
            return Err(Invalid(format!(
                "{what}: '{path}' has an unsafe component '{part}'"
            )));
        }
    }
    Ok(parts)
}

pub fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(at) => &path[..at],
    }
}

pub fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A symlink target is `(../)*name(/name)*`, with at most as many `..` as the
/// link is deep.
///
/// Every `..` comes first, so a target only climbs through the link's own
/// ancestors - which are real directories, because every entry's parent is -
/// and then descends. A descent through another link lands wherever that link
/// resolves, which is held to the same rule. So no chain of links leaves the
/// tree, and nothing that checks one ever has to follow it.
pub fn check_symlink_target(path: &str, target: &str, what: &str) -> Result<(), Invalid> {
    if target.is_empty() {
        return Err(Invalid(format!("{what}: symlink '{path}' has no target")));
    }
    if target.starts_with('/') {
        return Err(Invalid(format!(
            "{what}: symlink '{path}' -> '{target}' is absolute"
        )));
    }
    let parts: Vec<&str> = target.split('/').collect();
    let ups = parts.iter().take_while(|part| **part == "..").count();
    let names = &parts[ups..];
    if names.is_empty() {
        return Err(Invalid(format!(
            "{what}: symlink '{path}' -> '{target}' names no file"
        )));
    }
    for part in names {
        if matches!(*part, "" | "." | "..") || !is_name(part) {
            return Err(Invalid(format!(
                "{what}: symlink '{path}' -> '{target}' is not (../)*name(/name)*"
            )));
        }
    }
    let depth = path.matches('/').count() - 1;
    if ups > depth {
        return Err(Invalid(format!(
            "{what}: symlink '{path}' -> '{target}' leaves the tree"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_must_be_plain_and_absolute() {
        assert_eq!(split_path("/usr/lib", "t").unwrap(), ["usr", "lib"]);
        for bad in [
            "usr",
            "/",
            "/usr//lib",
            "/usr/./lib",
            "/usr/../etc",
            "/caf\u{e9}",
        ] {
            assert!(split_path(bad, "t").is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn symlink_targets_cannot_leave_the_tree() {
        assert!(check_symlink_target("/usr/lib/a", "b", "t").is_ok());
        assert!(check_symlink_target("/usr/lib/a", "../../bin/ls", "t").is_ok());
        for bad in [
            "/etc/passwd",
            "../../../etc",
            "a/../b",
            "./a",
            "..",
            "a//b",
            "",
        ] {
            assert!(
                check_symlink_target("/usr/lib/a", bad, "t").is_err(),
                "{bad} was accepted"
            );
        }
    }

    #[test]
    fn parents_and_names() {
        assert_eq!(parent_of("/usr"), "/");
        assert_eq!(parent_of("/usr/lib/x"), "/usr/lib");
        assert_eq!(name_of("/usr/lib/x"), "x");
    }
}
