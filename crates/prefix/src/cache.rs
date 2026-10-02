use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::format::{is_hex64, sha256_hex};

#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

/// `ROSEMARY_ROOTFS_CACHE`, then `$XDG_CACHE_HOME/rosemary/rootfs`, then
/// `$HOME/.cache/rosemary/rootfs` - the same shape as the shader cache's.
pub fn base_directory_from(
    override_directory: Option<OsString>,
    xdg_cache_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(directory) = override_directory.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(directory));
    }
    if let Some(cache) = xdg_cache_home.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(cache).join("rosemary/rootfs"));
    }
    let home = home.filter(|value| !value.is_empty())?;
    Some(PathBuf::from(home).join(".cache/rosemary/rootfs"))
}

/// One cached object, for `cache list`.
#[derive(Debug, Clone)]
pub struct Object {
    pub kind: &'static str,
    pub key: String,
    pub path: PathBuf,
    pub bytes: u64,
}

impl Cache {
    pub fn at(root: impl Into<PathBuf>) -> Cache {
        Cache { root: root.into() }
    }

    pub fn from_env() -> Option<Cache> {
        base_directory_from(
            std::env::var_os("ROSEMARY_ROOTFS_CACHE"),
            std::env::var_os("XDG_CACHE_HOME"),
            std::env::var_os("HOME"),
        )
        .map(Cache::at)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn artifact(&self, sha256: &str) -> PathBuf {
        self.root.join("artifacts").join(format!("{sha256}.tar.gz"))
    }

    pub fn manifest(&self, sha256: &str) -> PathBuf {
        self.root.join("manifests").join(format!("{sha256}.yaml"))
    }

    pub fn tree(&self, digest: &str) -> PathBuf {
        self.root.join("trees").join(digest)
    }

    /// A fresh directory for work in progress, on the same filesystem as the
    /// objects it will be renamed into.
    pub fn staging(&self) -> std::io::Result<PathBuf> {
        let base = self.root.join("staging");
        std::fs::create_dir_all(&base)?;
        for attempt in 0u32.. {
            let at = base.join(format!("{}-{attempt}", std::process::id()));
            match std::fs::create_dir(&at) {
                Ok(()) => return Ok(at),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        unreachable!("u32 attempts")
    }

    /// Move a finished file or tree into place as `dest`, replacing nothing:
    /// a concurrent writer that got there first wrote the same content.
    pub fn publish(&self, staged: &Path, dest: &Path) -> std::io::Result<()> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if dest.exists() {
            return remove_any(staged);
        }
        match std::fs::rename(staged, dest) {
            Ok(()) => Ok(()),
            // A tree someone else published in the meantime.
            Err(_) if dest.exists() => remove_any(staged),
            Err(e) => Err(e),
        }
    }

    /// Record that `prefix` uses this cache, so `gc` knows to look at it.
    pub fn register(&self, prefix: &Path) -> std::io::Result<()> {
        let refs = self.root.join("refs");
        std::fs::create_dir_all(&refs)?;
        let absolute = std::path::absolute(prefix)?;
        let id = sha256_hex(absolute.as_os_str().as_encoded_bytes());
        std::fs::write(
            refs.join(&id[..16]),
            absolute.as_os_str().as_encoded_bytes(),
        )
    }

    /// The prefixes registered here that still exist.
    pub fn prefixes(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.root.join("refs")) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            let path = PathBuf::from(<OsString as std::os::unix::ffi::OsStringExt>::from_vec(
                bytes,
            ));
            if crate::layout::created(&path) {
                out.push(path);
            } else {
                // A prefix that was deleted: its reference goes with it.
                let _ = std::fs::remove_file(entry.path());
            }
        }
        out.sort();
        out
    }

    pub fn list(&self) -> Vec<Object> {
        let mut out = Vec::new();
        for (kind, directory, suffix) in [
            ("artifact", "artifacts", ".tar.gz"),
            ("manifest", "manifests", ".yaml"),
            ("tree", "trees", ""),
        ] {
            let Ok(entries) = std::fs::read_dir(self.root.join(directory)) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(key) = name.strip_suffix(suffix).filter(|key| is_hex64(key)) else {
                    continue;
                };
                out.push(Object {
                    kind,
                    key: key.to_string(),
                    path: entry.path(),
                    bytes: size_of(&entry.path()),
                });
            }
        }
        out.sort_by(|a, b| (a.kind, &a.key).cmp(&(b.kind, &b.key)));
        out
    }

    /// Remove every object whose key is not in `keep`, and any abandoned
    /// staging. Only names this module could have made are touched - a
    /// 64-digit hex key with the right suffix - so nothing a user put here is.
    pub fn gc(&self, keep: &HashSet<String>) -> Vec<Object> {
        let mut removed = Vec::new();
        for object in self.list() {
            if keep.contains(&object.key) {
                continue;
            }
            if remove_any(&object.path).is_ok() {
                removed.push(object);
            }
        }
        if let Ok(entries) = std::fs::read_dir(self.root.join("staging")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some((pid, _)) = name.split_once('-') else {
                    continue;
                };
                let Ok(pid) = pid.parse::<libc::pid_t>() else {
                    continue;
                };
                // SAFETY: signal 0 only asks whether the process exists.
                let alive =
                    pid == std::process::id() as libc::pid_t || unsafe { libc::kill(pid, 0) } == 0;
                if !alive {
                    let _ = remove_any(&entry.path());
                }
            }
        }
        removed
    }
}

/// Remove a file, symlink or directory tree, making directories writable
/// first so a read-only tree does not stop it.
pub fn remove_any(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return std::fs::remove_file(path);
    }
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    for entry in std::fs::read_dir(path)? {
        remove_any(&entry?.path())?;
    }
    std::fs::remove_dir(path)
}

fn size_of(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .map(|entries| entries.flatten().map(|e| size_of(&e.path())).sum())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::tests::scratch;

    #[test]
    fn the_base_directory_prefers_the_override_then_xdg_then_home() {
        assert_eq!(
            base_directory_from(Some("/o".into()), Some("/x".into()), Some("/h".into())),
            Some(PathBuf::from("/o"))
        );
        assert_eq!(
            base_directory_from(None, Some("/x".into()), Some("/h".into())),
            Some(PathBuf::from("/x/rosemary/rootfs"))
        );
        assert_eq!(
            base_directory_from(None, Some("".into()), Some("/h".into())),
            Some(PathBuf::from("/h/.cache/rosemary/rootfs"))
        );
        assert_eq!(base_directory_from(None, None, None), None);
    }

    #[test]
    fn gc_keeps_what_is_referenced_and_touches_nothing_it_did_not_make() {
        let at = scratch("cache-gc");
        let cache = Cache::at(&at);
        let keep = "a".repeat(64);
        let drop = "b".repeat(64);
        for key in [&keep, &drop] {
            std::fs::create_dir_all(cache.tree(key)).unwrap();
            std::fs::create_dir_all(cache.artifact(key).parent().unwrap()).unwrap();
            std::fs::write(cache.artifact(key), b"x").unwrap();
        }
        let foreign = at.join("artifacts/notes.txt");
        std::fs::write(&foreign, b"mine").unwrap();

        let removed = cache.gc(&[keep.clone()].into());
        assert_eq!(removed.len(), 2);
        assert!(cache.tree(&keep).exists() && cache.artifact(&keep).exists());
        assert!(!cache.tree(&drop).exists() && !cache.artifact(&drop).exists());
        assert!(foreign.exists());
        let _ = std::fs::remove_dir_all(&at);
    }
}
