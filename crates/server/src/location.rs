//! Where a server listens.
//!
//! One server per prefix, as one wineserver per Wine prefix: processes in
//! different prefixes are different machines and must not see each other.
//! The socket is named for the prefix directory's device and inode rather than
//! its path, so two spellings of one prefix meet the same server and a prefix
//! moved elsewhere does not.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// This user's private runtime directory for Rosemary.
///
/// The same directory the bootstrap namespace lives under, and private for the
/// same reason: anyone able to listen on the socket would be the server.
pub fn runtime_directory() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => PathBuf::from(runtime).join("rosemary"),
        // SAFETY: `getuid` has no preconditions.
        None => PathBuf::from(format!("/tmp/rosemary-{}", unsafe { libc::getuid() })),
    }
}

/// The directory a server for `domain` keeps its socket and lock in; `None`
/// is the processes that run outside any prefix.
pub fn directory_for(domain: Option<&Path>) -> PathBuf {
    let leaf = match domain.and_then(|dir| std::fs::metadata(dir).ok()) {
        Some(meta) => format!("server-{:x}-{:x}", meta.dev(), meta.ino()),
        None => "server-host".to_string(),
    };
    runtime_directory().join(leaf)
}

pub fn socket_in(directory: &Path) -> PathBuf {
    directory.join("socket")
}

pub fn lock_in(directory: &Path) -> PathBuf {
    directory.join("lock")
}

/// Make `directory` private, along with Rosemary's runtime directory when used.
pub fn prepare(directory: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    let runtime = runtime_directory();
    let parent = directory.parent().filter(|parent| *parent == runtime);
    for dir in [parent, Some(directory)].into_iter().flatten() {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_custom_directory_leaves_its_parent_permissions_alone() {
        let parent =
            std::env::temp_dir().join(format!("rosemary-server-parent-{}", std::process::id()));
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o750)).unwrap();
        let directory = parent.join("server");
        prepare(&directory).unwrap();
        assert_eq!(std::fs::metadata(&parent).unwrap().mode() & 0o777, 0o750);
        assert_eq!(std::fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
        std::fs::remove_dir_all(parent).unwrap();
    }
}
