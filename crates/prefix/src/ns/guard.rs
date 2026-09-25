use std::collections::HashSet;
use std::ffi::CStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use crate::system::{self, SystemFiles};

struct Guard {
    /// The guest's `/`, as spelled and as resolved: a host whose home is
    /// behind a symlink reaches the same directory both ways.
    drives: Vec<PathBuf>,
    owned: HashSet<String>,
    policy: SystemFiles,
    dirty: PathBuf,
    seen: Mutex<HashSet<String>>,
}

fn guard() -> Option<&'static Guard> {
    static GUARD: LazyLock<Option<Guard>> = LazyLock::new(|| {
        let _errno = crate::errno::Held::now();
        let prefix = super::current()?;
        let provisioned = crate::provision::load(prefix).ok()?;
        let drive = prefix.drive();
        let mut drives = vec![drive.clone()];
        if let Ok(resolved) = std::fs::canonicalize(&drive)
            && resolved != drive
        {
            drives.push(resolved);
        }
        Some(Guard {
            drives,
            owned: provisioned
                .manifest
                .entries
                .iter()
                .map(|e| e.path.clone())
                .collect(),
            policy: provisioned.policy,
            dirty: system::dirty_path(prefix),
            seen: Mutex::new(HashSet::new()),
        })
    });
    GUARD.as_ref()
}

/// May the guest write to the host path `host`? `Err(errno)` means no.
///
/// A write that lands on a system file is recorded in the dirty list first,
/// under every policy that allows it.
pub fn may_write(host: &CStr) -> Result<(), libc::c_int> {
    may_write_at(libc::AT_FDCWD, host)
}

/// [`may_write`] for a path interpreted against an open directory.
pub fn may_write_at(dirfd: libc::c_int, host: &CStr) -> Result<(), libc::c_int> {
    // Identity mode, or a prefix with no base, has nothing to guard - and
    // answering that must not cost the guest its errno.
    if super::current().is_none() {
        return Ok(());
    }
    let _errno = crate::errno::Held::now();
    let Some(guard) = guard() else {
        return Ok(());
    };
    let Some(absolute) = absolute(
        dirfd,
        Path::new(std::ffi::OsStr::from_bytes(host.to_bytes())),
    ) else {
        return Ok(());
    };
    let Some(path) = guard.system_path(&absolute) else {
        return Ok(());
    };
    if guard.policy == SystemFiles::Immutable {
        return Err(libc::EROFS);
    }
    let first = guard
        .seen
        .lock()
        .map(|mut seen| seen.insert(path.clone()))
        .unwrap_or(true);
    if first {
        let _ = system::mark(&guard.dirty, &path);
        if guard.policy == SystemFiles::Warn {
            eprintln!("rosetta3: prefix: the guest is writing the system file {path}");
        }
    }
    Ok(())
}

impl Guard {
    /// The manifest path `absolute` names, if it is one the base owns.
    ///
    /// Checked twice: as spelled, and with its parent resolved, so that a
    /// write through a symlink the guest made elsewhere in the prefix is still
    /// seen for what it touches.
    fn system_path(&self, absolute: &Path) -> Option<String> {
        let lexical = normalise(absolute);
        if let Some(path) = self.owned_path(&lexical) {
            return Some(path);
        }
        let parent = lexical.parent()?;
        let name = lexical.file_name()?;
        let resolved = std::fs::canonicalize(parent).ok()?.join(name);
        self.owned_path(&resolved)
    }

    fn owned_path(&self, host: &Path) -> Option<String> {
        for drive in &self.drives {
            let Ok(rest) = host.strip_prefix(drive) else {
                continue;
            };
            let path = format!("/{}", rest.to_str()?);
            if self.owned.contains(&path) {
                return Some(path);
            }
        }
        None
    }
}

fn absolute(dirfd: libc::c_int, path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    if dirfd == libc::AT_FDCWD {
        return Some(std::env::current_dir().ok()?.join(path));
    }
    directory_of(dirfd).map(|directory| directory.join(path))
}

#[cfg(target_os = "linux")]
fn directory_of(dirfd: libc::c_int) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{dirfd}")).ok()
}

// FIXME (ACCURACY): no portable way to name a descriptor's directory here, so
// a write relative to one is not guarded off Linux.
#[cfg(not(target_os = "linux"))]
fn directory_of(_dirfd: libc::c_int) -> Option<PathBuf> {
    None
}

/// `.` and `..` resolved as text.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_dot_is_resolved_before_matching() {
        assert_eq!(
            normalise(Path::new("/p/root/usr/share/../lib/./x")),
            PathBuf::from("/p/root/usr/lib/x")
        );
    }

    #[test]
    fn a_guard_matches_under_either_spelling_of_the_drive() {
        let guard = Guard {
            drives: vec![
                PathBuf::from("/home/u/p/root"),
                PathBuf::from("/var/home/u/p/root"),
            ],
            owned: ["/usr/lib/libSystem.B.dylib".to_string()].into(),
            policy: SystemFiles::Repair,
            dirty: PathBuf::from("/dev/null"),
            seen: Mutex::new(HashSet::new()),
        };
        for spelled in [
            "/home/u/p/root/usr/lib/libSystem.B.dylib",
            "/var/home/u/p/root/usr/lib/libSystem.B.dylib",
            "/home/u/p/root/usr/share/../lib/libSystem.B.dylib",
        ] {
            assert_eq!(
                guard.system_path(Path::new(spelled)).as_deref(),
                Some("/usr/lib/libSystem.B.dylib"),
                "{spelled}"
            );
        }
        assert_eq!(
            guard.system_path(Path::new("/home/u/p/root/usr/lib/mine.dylib")),
            None
        );
        assert_eq!(
            guard.system_path(Path::new("/elsewhere/usr/lib/libSystem.B.dylib")),
            None
        );
    }
}
