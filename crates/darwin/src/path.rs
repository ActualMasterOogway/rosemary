//! Finding a file whose name a guest spelled in the wrong case.
//!
//! A Mac filesystem is case-*insensitive* by default, and Mac software relies on
//! it without meaning to: Deltarune's runner lowercases the path it derives from
//! its bundle and opens
//! `/.../deltarune.app/contents/resources/game.ios`, which is the same file on
//! HFS+ or APFS and nothing at all on ext4.
//!
//! So a path that does not exist is retried component by component against what
//! is actually on disk, and the walk is *only* a fallback: an exact match is
//! always taken first, so a directory that really does hold `Foo` and `foo` is
//! never guessed at for either of them. A component that matches more than one
//! entry ambiguously is not resolved at all - the real filesystem would have to
//! answer, and it cannot.
//!
//! Not needed on macOS, where the kernel already does this.

#![cfg(not(target_os = "macos"))]

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};

/// How many components a path may have before this stops walking it.
///
/// A path that deep is a loop or a fuzz case, and the walk is one `read_dir` per
/// component.
const MAX_COMPONENTS: usize = 64;

/// `path`, spelled the way the filesystem spells it, or `None` if no
/// case-insensitive reading of it exists either.
///
/// `None` for a path that already exists, because the caller is expected to have
/// tried it: this is the answer to "the exact name failed", not a replacement
/// for it.
///
/// `errno` is put back before returning. The caller has already failed a call
/// and is about to report that failure, so the walk's own syscalls must not be
/// what the guest reads: Rust's `read_dir` clears `errno` before each `readdir`
/// to tell the end of a directory from an error, so a walk that finds nothing
/// turns the caller's `ENOENT` into a failure with `errno == 0`. Boost reads
/// that as "not a not-found error" and throws `status: Success`.
pub fn insensitive(path: &CStr) -> Option<CString> {
    let _errno = crate::errno::Held::now();
    let text = path.to_str().ok()?;
    if text.is_empty() {
        return None;
    }
    let wanted = Path::new(text);
    let mut at = match wanted.is_absolute() {
        true => PathBuf::from("/"),
        false => PathBuf::from("."),
    };
    let mut components = 0;
    for part in wanted.components() {
        use std::path::Component;
        components += 1;
        if components > MAX_COMPONENTS {
            return None;
        }
        match part {
            Component::RootDir => continue,
            Component::CurDir => continue,
            Component::ParentDir => {
                at.push("..");
                continue;
            }
            Component::Prefix(_) => return None,
            Component::Normal(name) => {
                let exact = at.join(name);
                if exact.symlink_metadata().is_ok() {
                    at = exact;
                    continue;
                }
                at = match sole_match(&at, name.to_str()?) {
                    Some(found) => found,
                    None => return None,
                };
            }
        }
    }
    // A walk that ended where it started found nothing worth reporting.
    let found = at.to_str()?;
    (found != text).then(|| CString::new(found).ok())?
}

/// [`insensitive`] for a path interpreted relative to an open directory.
///
/// `/proc/self/fd` preserves `openat`'s starting point without changing the
/// process working directory. If procfs is unavailable, the ordinary exact
/// `openat` attempt has already happened and this fallback simply finds none.
pub fn insensitive_at(dirfd: libc::c_int, path: &CStr) -> Option<CString> {
    if path.to_bytes().starts_with(b"/") || dirfd == libc::AT_FDCWD {
        return insensitive(path);
    }
    let mut anchored = format!("/proc/self/fd/{dirfd}/").into_bytes();
    anchored.extend_from_slice(path.to_bytes());
    insensitive(&CString::new(anchored).ok()?)
}

/// The one entry of `directory` whose name equals `name` ignoring case.
///
/// `None` for none and for more than one. Two entries differing only in case is
/// a directory this cannot speak for, and picking either would be a coin toss
/// the caller has no way to notice.
fn sole_match(directory: &Path, name: &str) -> Option<PathBuf> {
    let mut found = None;
    for entry in std::fs::read_dir(directory).ok()?.flatten() {
        let candidate = entry.file_name();
        let candidate = candidate.to_str()?;
        if !candidate.eq_ignore_ascii_case(name) {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(entry.path());
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory with a known mixed-case name in it, removed when the test
    /// ends whichever way it ends.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> Scratch {
        let at = std::env::temp_dir().join(format!("r3-path-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(at.join("Contents")).expect("a scratch directory");
        std::fs::write(at.join("Contents").join("Game.ios"), b"x").expect("a file");
        Scratch(at)
    }

    #[test]
    fn a_lowercased_path_finds_the_file() {
        let scratch = scratch("lower");
        let lowered = CString::new(
            scratch
                .0
                .join("contents")
                .join("game.ios")
                .to_str()
                .expect("utf-8"),
        )
        .expect("no NUL");
        let found = insensitive(&lowered).expect("a case-insensitive match");
        assert_eq!(
            found.to_str().expect("utf-8"),
            scratch
                .0
                .join("Contents")
                .join("Game.ios")
                .to_str()
                .unwrap()
        );
    }

    /// A path that is already right is not "resolved" to itself: the caller has
    /// tried it, and answering would send it round the same failure twice.
    #[test]
    fn an_exact_path_is_not_a_match() {
        let scratch = scratch("exact");
        let exact = CString::new(
            scratch
                .0
                .join("Contents")
                .join("Game.ios")
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(insensitive(&exact), None);
    }

    #[test]
    fn a_path_that_is_not_there_in_any_case_is_none() {
        let scratch = scratch("absent");
        let absent =
            CString::new(scratch.0.join("contents").join("nothing").to_str().unwrap()).unwrap();
        assert_eq!(insensitive(&absent), None);
    }

    /// The walk reads directories, and `read_dir` clears `errno` on the way. The
    /// caller is reporting the failure that sent it here, so that must not show.
    #[test]
    fn the_walk_leaves_errno_where_it_found_it() {
        let scratch = scratch("errno");
        let absent =
            CString::new(scratch.0.join("Contents").join("nothing").to_str().unwrap()).unwrap();
        crate::errno::set(libc::ENOENT);
        assert_eq!(insensitive(&absent), None);
        assert_eq!(crate::errno::get(), libc::ENOENT);
        crate::errno::set(0);
    }

    #[test]
    fn a_relative_path_can_start_at_a_directory_descriptor() {
        let scratch = scratch("at");
        let directory = CString::new(scratch.0.to_str().expect("utf-8")).expect("no NUL");
        // SAFETY: a NUL-terminated path and native flags.
        let dirfd = unsafe { libc::open(directory.as_ptr(), libc::O_RDONLY) };
        assert!(dirfd >= 0);
        let found = insensitive_at(dirfd, c"contents/game.ios")
            .expect("a case-insensitive relative match");
        // SAFETY: a descriptor this test just opened.
        unsafe { libc::close(dirfd) };

        assert!(found.to_bytes().ends_with(b"/Contents/Game.ios"));
    }
}
