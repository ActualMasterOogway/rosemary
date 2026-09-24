//! Put a prefix's system files back the way the base has them.
//!
//! Transactional, in two passes. First every replacement is *staged* under
//! `.rosetta3/repair/` from the verified, cached base - so no network - and
//! only then is each one renamed into place. Nothing is modified while the tree
//! is still being read, and each rename is atomic, so a crash leaves either the
//! old file or the known-good one, never half of each.
//!
//! Provisioning a new prefix is the same operation: a repair of a tree in which
//! everything is missing.
//!
//! Nothing a user made is deleted. Something of the wrong type sitting where a
//! system file belongs - a directory where a dylib should be, say - may hold
//! the user's data, so it is moved aside to `.rosetta3/displaced/` instead.

use std::collections::BTreeSet;
use std::fs::File;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::format::manifest::{Kind, Manifest};
use crate::format::parent_of;
use crate::layout::Prefix;
use crate::verify::{Difference, Problem, host};

/// How a system file was put into a prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Method {
    /// A copy-on-write clone of the cached file: no extra space, and a write to
    /// it does not reach the cache.
    Reflink,
    /// The cached file itself. Only under `system_files: immutable`, where a
    /// guest's writes are refused anyway: otherwise one writing in place would
    /// corrupt the base every other prefix shares.
    Hardlink,
    Copy,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Method::Reflink => "reflink",
            Method::Hardlink => "hardlink",
            Method::Copy => "copy",
        }
    }
}

#[derive(Debug, Default)]
pub struct Outcome {
    /// Paths put back.
    pub restored: Vec<String>,
    /// Paths that were in the way, and where they went.
    pub displaced: Vec<(String, PathBuf)>,
    pub methods: BTreeSet<Method>,
}

/// Which differences a repair acts on.
pub fn actionable(problems: &[Problem]) -> impl Iterator<Item = &Problem> {
    problems.iter().filter(|p| p.kind != Difference::Unexpected)
}

/// Hold the prefix's lock for as long as the value lives, so two processes
/// never repair or provision one prefix at once.
pub fn lock(prefix: &Prefix) -> io::Result<File> {
    std::fs::create_dir_all(prefix.control())?;
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(prefix.control().join("lock"))?;
    use std::os::fd::AsRawFd;
    // SAFETY: a descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

/// Repair every problem in `problems` (in manifest order, as
/// [`crate::verify::compare`] gives them) under `root`, from the pristine tree
/// at `base`. `displace` additionally moves the listed unexpected paths aside,
/// for `reset-system`.
pub fn apply(
    prefix: &Prefix,
    root: &Path,
    manifest: &Manifest,
    base: &Path,
    problems: &[Problem],
    hardlinks: bool,
) -> io::Result<Outcome> {
    let staging = prefix.control().join("repair");
    // A previous repair that crashed left only staged copies behind; the tree
    // itself is whole, so they are simply discarded.
    if staging.exists() {
        crate::cache::remove_any(&staging)?;
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&staging)?;

    let mut outcome = Outcome::default();
    let mut work: Vec<(&Problem, Option<PathBuf>)> = Vec::new();

    // Pass one: stage. Reads the tree only.
    let mut previous: Option<&str> = None;
    for (index, problem) in problems.iter().enumerate() {
        // A file can be both `modified` and `mode`; one replacement fixes both.
        if previous == Some(problem.path.as_str()) {
            continue;
        }
        previous = Some(&problem.path);
        match problem.kind {
            Difference::Unexpected => {
                work.push((problem, None));
                continue;
            }
            Difference::Missing
            | Difference::Modified
            | Difference::WrongType
            | Difference::Mode => {}
        }
        let Some(entry) = manifest.get(&problem.path) else {
            continue;
        };
        let source = host(base, &entry.path);
        let staged = staging.join(index.to_string());
        let staged = match &entry.kind {
            Kind::Directory { .. } => None,
            // A file whose mode alone is wrong is replaced too, rather than
            // `chmod`ed: under hardlinks the inode is the cache's.
            Kind::File { mode, .. } => {
                outcome
                    .methods
                    .insert(materialise(&source, &staged, *mode, hardlinks)?);
                Some(staged)
            }
            Kind::Symlink { target } => {
                std::os::unix::fs::symlink(target, &staged)?;
                Some(staged)
            }
        };
        work.push((problem, staged));
    }

    // Pass two: move into place, parents first.
    let displaced_root = prefix.control().join("displaced").join(stamp());
    for (problem, staged) in work {
        let at = host(root, &problem.path);
        if problem.kind == Difference::Unexpected {
            outcome.displaced.push((
                problem.path.clone(),
                displace(&at, &displaced_root, &problem.path)?,
            ));
            continue;
        }
        let Some(entry) = manifest.get(&problem.path) else {
            continue;
        };
        if problem.kind == Difference::WrongType {
            outcome.displaced.push((
                problem.path.clone(),
                displace(&at, &displaced_root, &problem.path)?,
            ));
        }
        match (&entry.kind, staged) {
            (Kind::Directory { mode, .. }, _) => {
                if problem.kind == Difference::Mode {
                    std::fs::set_permissions(&at, std::fs::Permissions::from_mode(*mode))?;
                } else {
                    match std::fs::DirBuilder::new().mode(*mode).create(&at) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(annotate(e, &at)),
                    }
                    // The umask may have taken bits off.
                    std::fs::set_permissions(&at, std::fs::Permissions::from_mode(*mode))?;
                }
            }
            (_, Some(staged)) => {
                std::fs::rename(&staged, &at).map_err(|e| annotate(e, &at))?;
            }
            (_, None) => continue,
        }
        outcome.restored.push(problem.path.clone());
    }
    let _ = std::fs::remove_dir_all(&staging);
    Ok(outcome)
}

/// Remove what an older base put here and a newer one does not, when it is
/// still exactly what the older base put there. A file somebody changed is
/// theirs now, and stays.
pub fn retire(root: &Path, old: &Manifest, new: &Manifest) -> io::Result<Vec<String>> {
    let mut removed = Vec::new();
    for entry in old.entries.iter().rev() {
        if new.contains(&entry.path) {
            continue;
        }
        let at = host(root, &entry.path);
        let Ok(meta) = std::fs::symlink_metadata(&at) else {
            continue;
        };
        let unchanged = match &entry.kind {
            Kind::Directory { .. } => meta.is_dir(),
            _ => {
                let single = [entry.path.clone()].into();
                meta.file_type().is_file() == matches!(entry.kind, Kind::File { .. })
                    && crate::verify::compare(
                        root,
                        old,
                        true,
                        &crate::verify::Scope::Paths(&single),
                    )?
                    .iter()
                    .all(|p| p.path != entry.path)
            }
        };
        if !unchanged {
            continue;
        }
        let result = match meta.is_dir() {
            // Only if empty: anything left inside is somebody's.
            true => std::fs::remove_dir(&at),
            false => std::fs::remove_file(&at),
        };
        if result.is_ok() {
            removed.push(entry.path.clone());
        }
    }
    Ok(removed)
}

/// Make `dest` a copy of the cached file `source` with `mode`, as cheaply as
/// this filesystem allows.
fn materialise(source: &Path, dest: &Path, mode: u32, hardlinks: bool) -> io::Result<Method> {
    if reflink(source, dest).is_ok() {
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode))?;
        return Ok(Method::Reflink);
    }
    let _ = std::fs::remove_file(dest);
    if hardlinks && std::fs::hard_link(source, dest).is_ok() {
        return Ok(Method::Hardlink);
    }
    std::fs::copy(source, dest).map_err(|e| annotate(e, source))?;
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode))?;
    Ok(Method::Copy)
}

#[cfg(target_os = "linux")]
fn reflink(source: &Path, dest: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // `_IOW(0x94, 9, int)`, from <linux/fs.h>.
    const FICLONE: u32 = 0x4004_9409;
    let from = File::open(source)?;
    let to = File::create_new(dest)?;
    // SAFETY: two descriptors this function owns; FICLONE takes the source
    // descriptor by value.
    let rc = unsafe { libc::ioctl(to.as_raw_fd(), FICLONE as _, from.as_raw_fd()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn reflink(_source: &Path, _dest: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Move whatever is at `at` into `displaced_root`, keeping its path, and say
/// where it went.
fn displace(at: &Path, displaced_root: &Path, path: &str) -> io::Result<PathBuf> {
    let dest = host(displaced_root, path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(at, &dest).map_err(|e| annotate(e, at))?;
    Ok(dest)
}

fn stamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{seconds}-{}", std::process::id())
}

fn annotate(error: io::Error, at: &Path) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", at.display()))
}

/// The unexpected paths `reset-system` moves aside: those directly inside a
/// directory the base owns. `/Users` and `/Applications` sit in `/`, which no
/// manifest lists, and are never touched.
pub fn strays<'a>(problems: &'a [Problem], manifest: &Manifest) -> Vec<&'a Problem> {
    problems
        .iter()
        .filter(|p| p.kind == Difference::Unexpected && manifest.contains(parent_of(&p.path)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::tests::{materialise_sample, scratch};
    use crate::verify::{Scope, compare};

    fn prefix_with_base(name: &str) -> (PathBuf, Prefix, Manifest, PathBuf) {
        let at = scratch(name);
        let base = at.join("base");
        std::fs::create_dir(&base).unwrap();
        let manifest = materialise_sample(&base);
        let prefix = Prefix::at(at.join("prefix"));
        std::fs::create_dir_all(prefix.drive()).unwrap();
        (at, prefix, manifest, base)
    }

    #[test]
    fn provisioning_is_a_repair_of_an_empty_tree() {
        let (at, prefix, manifest, base) = prefix_with_base("repair-provision");
        let root = prefix.drive();
        let problems = compare(&root, &manifest, true, &Scope::All).unwrap();
        assert_eq!(problems.len(), manifest.entries.len());
        let outcome = apply(&prefix, &root, &manifest, &base, &problems, false).unwrap();
        assert_eq!(outcome.restored.len(), manifest.entries.len());
        assert_eq!(compare(&root, &manifest, true, &Scope::All).unwrap(), []);
        assert!(!prefix.control().join("repair").exists());
        let _ = std::fs::remove_dir_all(&at);
    }

    /// The plan's end-to-end case, in miniature: break it every way, heal it
    /// offline, and leave the user's own file alone.
    #[test]
    fn every_kind_of_damage_heals_and_user_files_survive() {
        let (at, prefix, manifest, base) = prefix_with_base("repair-heal");
        let root = prefix.drive();
        let problems = compare(&root, &manifest, true, &Scope::All).unwrap();
        apply(&prefix, &root, &manifest, &base, &problems, false).unwrap();

        std::fs::write(root.join("usr/lib/data.txt"), b"broken").unwrap();
        std::fs::remove_file(root.join("usr/lib/libA.dylib")).unwrap();
        std::fs::remove_file(root.join("usr/lib/libB.dylib")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("usr/lib/libB.dylib")).unwrap();
        std::fs::set_permissions(
            root.join("usr/share/doc/readme"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::fs::remove_dir_all(root.join("usr/share/doc")).unwrap();
        std::fs::write(root.join("usr/share/doc"), b"a file where a directory was").unwrap();
        std::fs::write(root.join("usr/lib/mine.dylib"), b"mine").unwrap();

        let problems = compare(&root, &manifest, true, &Scope::All).unwrap();
        assert!(problems.iter().any(|p| p.kind == Difference::WrongType));
        let outcome = apply(&prefix, &root, &manifest, &base, &problems, false).unwrap();
        assert_eq!(compare(&root, &manifest, true, &Scope::All).unwrap(), []);
        assert_eq!(
            std::fs::read(root.join("usr/lib/mine.dylib")).unwrap(),
            b"mine"
        );
        // The file that sat where a directory belongs was moved, not deleted.
        let (_, moved) = &outcome.displaced[0];
        assert_eq!(
            std::fs::read(moved).unwrap(),
            b"a file where a directory was"
        );
        let _ = std::fs::remove_dir_all(&at);
    }

    #[test]
    fn a_repair_never_writes_through_a_redirected_directory() {
        let (at, prefix, manifest, base) = prefix_with_base("repair-redirect");
        let root = prefix.drive();
        let problems = compare(&root, &manifest, true, &Scope::All).unwrap();
        apply(&prefix, &root, &manifest, &base, &problems, false).unwrap();
        let elsewhere = at.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::remove_dir_all(root.join("usr/lib")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("usr/lib")).unwrap();

        let problems = compare(&root, &manifest, true, &Scope::All).unwrap();
        apply(&prefix, &root, &manifest, &base, &problems, false).unwrap();
        assert_eq!(compare(&root, &manifest, true, &Scope::All).unwrap(), []);
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&at);
    }

    #[test]
    fn hardlinks_share_the_cached_inode() {
        use std::os::unix::fs::MetadataExt;
        let (at, prefix, manifest, base) = prefix_with_base("repair-hardlink");
        let root = prefix.drive();
        let problems = compare(&root, &manifest, true, &Scope::All).unwrap();
        let outcome = apply(&prefix, &root, &manifest, &base, &problems, true).unwrap();
        if outcome.methods.contains(&Method::Hardlink) {
            let a = std::fs::metadata(root.join("usr/lib/data.txt")).unwrap();
            let b = std::fs::metadata(base.join("usr/lib/data.txt")).unwrap();
            assert_eq!(a.ino(), b.ino());
        }
        let _ = std::fs::remove_dir_all(&at);
    }
}
