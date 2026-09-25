use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::format::manifest::{Entry, Kind};
use crate::format::{hex, name_of, parent_of, sha256_hex};

pub const ALGORITHM: &str = "sha256-merkle-v1";

/// The tree digest, and every directory's own digest, for entries in canonical
/// order (every parent before its children).
pub fn merkle(entries: &[Entry]) -> (String, HashMap<String, String>) {
    let mut children: HashMap<&str, Vec<usize>> = HashMap::new();
    children.insert("/", Vec::new());
    for (index, entry) in entries.iter().enumerate() {
        children
            .entry(parent_of(&entry.path))
            .or_default()
            .push(index);
        if matches!(entry.kind, Kind::Directory { .. }) {
            children.entry(&entry.path).or_default();
        }
    }
    let mut digests: HashMap<String, String> = HashMap::new();
    let directory = |path: &str, digests: &HashMap<String, String>| -> String {
        let mut members: Vec<usize> = children.get(path).cloned().unwrap_or_default();
        members.sort_by(|a, b| {
            name_of(&entries[*a].path)
                .as_bytes()
                .cmp(name_of(&entries[*b].path).as_bytes())
        });
        let mut hasher = Sha256::new();
        for index in members {
            hasher.update(record(&entries[index], digests));
        }
        hex(&hasher.finalize())
    };
    // Reversed canonical order visits every directory after all of its
    // descendants, so each child's digest is known by the time it is needed.
    for entry in entries.iter().rev() {
        if matches!(entry.kind, Kind::Directory { .. }) {
            let digest = directory(&entry.path, &digests);
            digests.insert(entry.path.clone(), digest);
        }
    }
    (directory("/", &digests), digests)
}

fn record(entry: &Entry, digests: &HashMap<String, String>) -> Vec<u8> {
    let name = name_of(&entry.path);
    let line = match &entry.kind {
        Kind::File { mode, sha256, .. } => format!("file {mode:04o} {sha256} {name}"),
        Kind::Symlink { target } => format!("symlink - {} {name}", sha256_hex(target.as_bytes())),
        Kind::Directory { mode, .. } => {
            let digest = digests.get(&entry.path).map(String::as_str).unwrap_or("");
            format!("directory {mode:04o} {digest} {name}")
        }
    };
    let mut bytes = line.into_bytes();
    bytes.push(0);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, mode: u32, content: &[u8]) -> Entry {
        Entry {
            path: path.into(),
            kind: Kind::File {
                mode,
                size: content.len() as u64,
                sha256: sha256_hex(content),
            },
        }
    }

    fn directory(path: &str) -> Entry {
        Entry {
            path: path.into(),
            kind: Kind::Directory {
                mode: 0o755,
                digest: String::new(),
            },
        }
    }

    /// The vector `docs/rootfs-spec.md` publishes. It catches every ordering,
    /// separator and mode-formatting mistake at once.
    #[test]
    fn matches_the_published_test_vector() {
        let entries = vec![
            directory("/usr"),
            directory("/usr/lib"),
            file("/usr/lib/data.txt", 0o644, b"data\n"),
            file(
                "/usr/lib/libA.dylib",
                0o755,
                b"\xcf\xfa\xed\xfe not really a mach-o\n",
            ),
            Entry {
                path: "/usr/lib/libB.dylib".into(),
                kind: Kind::Symlink {
                    target: "libA.dylib".into(),
                },
            },
            directory("/usr/share"),
            directory("/usr/share/doc"),
            file("/usr/share/doc/readme", 0o644, b""),
        ];
        let (tree, _) = merkle(&entries);
        assert_eq!(
            tree,
            "1cb4990e4557b548759f593c103a1a05e0d4558f9c5fb0e3b53969165ae88852"
        );
    }

    /// Children are ordered by name, not by where they appear in the list.
    #[test]
    fn the_digest_does_not_depend_on_listing_order() {
        let a = vec![
            directory("/d"),
            file("/d/B", 0o644, b"1"),
            file("/d/a", 0o644, b"2"),
        ];
        let b = vec![
            directory("/d"),
            file("/d/a", 0o644, b"2"),
            file("/d/B", 0o644, b"1"),
        ];
        assert_eq!(merkle(&a).0, merkle(&b).0);
    }
}
