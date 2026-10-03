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
}
