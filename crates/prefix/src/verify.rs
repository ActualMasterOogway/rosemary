use std::collections::HashSet;
use std::fmt;
use std::fs::Metadata;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::format::manifest::{Entry, Kind, Manifest};
use crate::format::{parent_of, sha256_reader};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Difference {
    /// The path does not exist, or its parent is no longer a real directory.
    Missing,
    /// The file's size or content differs, or the symlink's target does.
    Modified,
    /// The path is not the type the manifest says.
    WrongType,
    /// The permissions differ.
    Mode,
    /// The path is not in the manifest. Never reported for a prefix, which is
    /// full of files a user made.
    Unexpected,
}

impl Difference {
    pub const ALL: [Difference; 5] = [
        Difference::Missing,
        Difference::Modified,
        Difference::WrongType,
        Difference::Mode,
        Difference::Unexpected,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Difference::Missing => "missing",
            Difference::Modified => "modified",
            Difference::WrongType => "wrong-type",
            Difference::Mode => "mode",
            Difference::Unexpected => "unexpected",
        }
    }
}

impl fmt::Display for Difference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub kind: Difference,
    /// The manifest's spelling: absolute, from the tree's root.
    pub path: String,
    pub detail: String,
}

/// Where the manifest path `path` is under `root`.
pub fn host(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

/// Which entries a comparison looks at.
pub enum Scope<'a> {
    /// Every entry.
    All,
    /// These paths, their ancestors, and everything below them: the targeted
    /// check after a write was seen landing on a system file.
    Paths(&'a HashSet<String>),
}

impl Scope<'_> {
    fn includes(&self, path: &str) -> bool {
        let Scope::Paths(paths) = self else {
            return true;
        };
        if paths.contains(path) {
            return true;
        }
        // Below one of them.
        let mut at = path;
        while at != "/" {
            at = parent_of(at);
            if paths.contains(at) {
                return true;
            }
        }
        // Above one of them: an ancestor that is no longer a directory makes
        // everything below it missing, so it has to be looked at too.
        let prefix = format!("{path}/");
        paths.iter().any(|p| p.starts_with(&prefix))
    }
}

/// Every difference between the tree at `root` and `manifest`, in manifest
/// order. `allow_extra` suppresses [`Difference::Unexpected`].
pub fn compare(
    root: &Path,
    manifest: &Manifest,
    allow_extra: bool,
    scope: &Scope,
) -> io::Result<Vec<Problem>> {
    let mut problems = Vec::new();
    let mut directories: HashSet<&str> = ["/"].into();
    // Parents that were not examined because they were out of scope, and so
    // are assumed to be what they claim until something says otherwise.
    let mut skipped: HashSet<&str> = HashSet::new();
    for entry in &manifest.entries {
        let path = entry.path.as_str();
        if !scope.includes(path) {
            if matches!(entry.kind, Kind::Directory { .. }) {
                skipped.insert(path);
            }
            continue;
        }
        let parent = parent_of(path);
        if !directories.contains(parent) && !skipped.contains(parent) {
            problems.push(problem(
                Difference::Missing,
                path,
                "parent is not a directory",
            ));
            continue;
        }
        let at = host(root, path);
        let meta = match std::fs::symlink_metadata(&at) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                problems.push(problem(Difference::Missing, path, ""));
                continue;
            }
            // A parent that is a file, not a directory: the path cannot exist.
            Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => {
                problems.push(problem(Difference::Missing, path, ""));
                continue;
            }
            Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", at.display()))),
        };
        if check(entry, &at, &meta, &mut problems)? && matches!(entry.kind, Kind::Directory { .. })
        {
            directories.insert(path);
        }
    }
    if !allow_extra {
        walk(root, "/", manifest, &directories, &mut problems)?;
    }
    Ok(problems)
}

/// Compare one entry that exists. `true` when it is the right type, which for
/// a directory means its contents can be looked at.
fn check(
    entry: &Entry,
    at: &Path,
    meta: &Metadata,
    problems: &mut Vec<Problem>,
) -> io::Result<bool> {
    let path = &entry.path;
    let file_type = meta.file_type();
    let mode = meta.mode() & 0o7777;
    match &entry.kind {
        Kind::Directory { mode: expected, .. } => {
            if !file_type.is_dir() {
                problems.push(problem(Difference::WrongType, path, "expected a directory"));
                return Ok(false);
            }
            if mode != *expected {
                problems.push(problem(
                    Difference::Mode,
                    path,
                    &format!("{mode:04o}, expected {expected:04o}"),
                ));
            }
        }
        Kind::File {
            mode: expected,
            size,
            sha256,
        } => {
            if !file_type.is_file() {
                problems.push(problem(
                    Difference::WrongType,
                    path,
                    "expected a regular file",
                ));
                return Ok(false);
            }
            if meta.len() != *size {
                problems.push(problem(
                    Difference::Modified,
                    path,
                    &format!("{} bytes, expected {size}", meta.len()),
                ));
            } else if hash_nofollow(at)?.as_deref() != Some(sha256.as_str()) {
                problems.push(problem(Difference::Modified, path, "content differs"));
            }
            if mode != *expected {
                problems.push(problem(
                    Difference::Mode,
                    path,
                    &format!("{mode:04o}, expected {expected:04o}"),
                ));
            }
        }
        Kind::Symlink { target } => {
            if !file_type.is_symlink() {
                problems.push(problem(Difference::WrongType, path, "expected a symlink"));
                return Ok(false);
            }
            let actual = std::fs::read_link(at)?;
            if actual.as_os_str().as_encoded_bytes() != target.as_bytes() {
                problems.push(problem(
                    Difference::Modified,
                    path,
                    &format!("-> {}, expected -> {target}", actual.display()),
                ));
            }
        }
    }
    Ok(true)
}

/// A regular file's SHA-256, or `None` if what is there now is not one. Opened
/// with `O_NOFOLLOW`, so a file swapped for a link between the `lstat` and the
/// open is not hashed through it.
fn hash_nofollow(at: &Path) -> io::Result<Option<String>> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(at)
    {
        Ok(file) => file,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.file_type().is_file() {
        return Ok(None);
    }
    Ok(Some(sha256_reader(file)?.0))
}

fn walk(
    root: &Path,
    guest: &str,
    manifest: &Manifest,
    directories: &HashSet<&str>,
    problems: &mut Vec<Problem>,
) -> io::Result<()> {
    let mut names: Vec<Vec<u8>> = std::fs::read_dir(host(root, guest))?
        .map(|entry| entry.map(|e| e.file_name().into_encoded_bytes()))
        .collect::<io::Result<_>>()?;
    names.sort();
    for name in names {
        let name = String::from_utf8_lossy(&name);
        let path = match guest {
            "/" => format!("/{name}"),
            _ => format!("{guest}/{name}"),
        };
        if !manifest.contains(&path) {
            problems.push(problem(Difference::Unexpected, &path, ""));
        } else if directories.contains(path.as_str()) {
            walk(root, &path, manifest, directories, problems)?;
        }
    }
    Ok(())
}

fn problem(kind: Difference, path: &str, detail: &str) -> Problem {
    Problem {
        kind,
        path: path.to_string(),
        detail: detail.to_string(),
    }
}

/// Counts per kind, in [`Difference::ALL`] order.
pub fn counts(problems: &[Problem]) -> [usize; 5] {
    let mut counts = [0; 5];
    for p in problems {
        let index = Difference::ALL
            .iter()
            .position(|k| *k == p.kind)
            .expect("every kind is listed");
        counts[index] += 1;
    }
    counts
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::format::manifest::tests::sample;
    use std::os::unix::fs::PermissionsExt;

    pub(crate) fn scratch(name: &str) -> PathBuf {
        let at =
            std::env::temp_dir().join(format!("rosemary-prefix-{name}-{}", std::process::id()));
        let _ = make_writable_and_remove(&at);
        std::fs::create_dir_all(&at).expect("scratch directory");
        at
    }

    pub(crate) fn make_writable_and_remove(at: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(at)
    }

    /// Write the sample manifest's tree to `root`.
    pub(crate) fn materialise_sample(root: &Path) -> Manifest {
        let (manifest, files) = sample();
        for entry in &manifest.entries {
            let at = host(root, &entry.path);
            match &entry.kind {
                Kind::Directory { mode, .. } => {
                    std::fs::create_dir(&at).unwrap();
                    std::fs::set_permissions(&at, std::fs::Permissions::from_mode(*mode)).unwrap();
                }
                Kind::File { mode, .. } => {
                    let content = files.iter().find(|(p, _)| *p == entry.path).unwrap().1;
                    std::fs::write(&at, content).unwrap();
                    std::fs::set_permissions(&at, std::fs::Permissions::from_mode(*mode)).unwrap();
                }
                Kind::Symlink { target } => std::os::unix::fs::symlink(target, &at).unwrap(),
            }
        }
        manifest
    }

    fn kinds(problems: &[Problem]) -> Vec<(Difference, &str)> {
        problems.iter().map(|p| (p.kind, p.path.as_str())).collect()
    }

    #[test]
    fn a_pristine_tree_has_no_differences() {
        let root = scratch("verify-clean");
        let manifest = materialise_sample(&root);
        assert_eq!(compare(&root, &manifest, false, &Scope::All).unwrap(), []);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The checkpoint from the plan: each way of breaking a file is reported
    /// as exactly its own kind.
    #[test]
    fn each_kind_of_damage_is_reported_as_itself() {
        let root = scratch("verify-damage");
        let manifest = materialise_sample(&root);
        // Truncated.
        std::fs::write(root.join("usr/lib/data.txt"), b"").unwrap();
        // Same size, different content.
        std::fs::write(
            root.join("usr/lib/libA.dylib"),
            b"\xcf\xfa\xed\xfe NOT really a mach-o\n",
        )
        .unwrap();
        // Deleted.
        std::fs::remove_file(root.join("usr/share/doc/readme")).unwrap();
        // Retargeted.
        std::fs::remove_file(root.join("usr/lib/libB.dylib")).unwrap();
        std::os::unix::fs::symlink("data.txt", root.join("usr/lib/libB.dylib")).unwrap();
        // chmod.
        std::fs::set_permissions(
            root.join("usr/share"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        // Unexpected.
        std::fs::write(root.join("usr/lib/mine.dylib"), b"mine").unwrap();

        let found = compare(&root, &manifest, false, &Scope::All).unwrap();
        assert_eq!(
            kinds(&found),
            [
                (Difference::Modified, "/usr/lib/data.txt"),
                (Difference::Modified, "/usr/lib/libA.dylib"),
                (Difference::Modified, "/usr/lib/libB.dylib"),
                (Difference::Mode, "/usr/share"),
                (Difference::Missing, "/usr/share/doc/readme"),
                (Difference::Unexpected, "/usr/lib/mine.dylib"),
            ]
        );
        // A prefix full of user files does not report the last one.
        let found = compare(&root, &manifest, true, &Scope::All).unwrap();
        assert!(found.iter().all(|p| p.kind != Difference::Unexpected));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A directory replaced by a link to a perfect copy is not verified: it
    /// is the wrong type, and nothing under it is looked at.
    #[test]
    fn a_symlink_is_never_followed() {
        let root = scratch("verify-follow");
        let manifest = materialise_sample(&root);
        let copy = scratch("verify-follow-copy");
        materialise_sample(&copy);
        std::fs::remove_dir_all(root.join("usr/lib")).unwrap();
        std::os::unix::fs::symlink(copy.join("usr/lib"), root.join("usr/lib")).unwrap();

        let found = compare(&root, &manifest, true, &Scope::All).unwrap();
        assert_eq!(
            kinds(&found),
            [
                (Difference::WrongType, "/usr/lib"),
                (Difference::Missing, "/usr/lib/data.txt"),
                (Difference::Missing, "/usr/lib/libA.dylib"),
                (Difference::Missing, "/usr/lib/libB.dylib"),
            ]
        );
        // A file swapped for a symlink to its own pristine content likewise.
        std::fs::remove_file(root.join("usr/share/doc/readme")).unwrap();
        std::os::unix::fs::symlink(
            copy.join("usr/share/doc/readme"),
            root.join("usr/share/doc/readme"),
        )
        .unwrap();
        let found = compare(&root, &manifest, true, &Scope::All).unwrap();
        assert!(found.contains(&Problem {
            kind: Difference::WrongType,
            path: "/usr/share/doc/readme".into(),
            detail: "expected a regular file".into(),
        }));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&copy);
    }

    #[test]
    fn a_targeted_check_looks_only_where_it_was_told() {
        let root = scratch("verify-targeted");
        let manifest = materialise_sample(&root);
        std::fs::write(root.join("usr/lib/data.txt"), b"x").unwrap();
        std::fs::write(root.join("usr/share/doc/readme"), b"x").unwrap();
        let paths: HashSet<String> = ["/usr/share/doc".to_string()].into();
        let found = compare(&root, &manifest, true, &Scope::Paths(&paths)).unwrap();
        assert_eq!(
            kinds(&found),
            [(Difference::Modified, "/usr/share/doc/readme")]
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
