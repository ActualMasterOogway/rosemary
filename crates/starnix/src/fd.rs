//! Open descriptors.

use std::os::fd::RawFd;
use std::path::PathBuf;

use crate::sys;

/// A path that reaches what `fd` has open, for APIs that only take paths.
///
/// Where the host has a link that follows the descriptor itself - Linux's
/// `/proc/self/fd` - that is what comes back, so a renamed or unlinked target
/// is still reached. Elsewhere it is the name the kernel last knew.
pub fn path(fd: RawFd) -> Option<PathBuf> {
    sys::fd_path(fd)
}

/// Where `fd` points now, as an ordinary path that can be compared with others.
///
/// [`path`] may be a link that only works in this process; this is the place
/// it leads to, `None` once that is gone or unknown.
pub fn resolved(fd: RawFd) -> Option<PathBuf> {
    sys::fd_target(fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn a_directory_descriptor_reaches_its_entries() {
        let dir = std::env::temp_dir().join(format!("starnix-fd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("entry"), b"x").unwrap();
        let handle = std::fs::File::open(&dir).unwrap();
        let path = path(handle.as_raw_fd()).unwrap();
        assert_eq!(std::fs::read(path.join("entry")).unwrap(), b"x");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_descriptor_resolves_to_its_file() {
        let file = std::env::temp_dir().join(format!("starnix-fdr-{}", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let handle = std::fs::File::open(&file).unwrap();
        let resolved = resolved(handle.as_raw_fd()).unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            file.canonicalize().unwrap()
        );
        std::fs::remove_file(&file).unwrap();
    }
}
