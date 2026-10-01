//! What a directory listing has to change to show the mount table.
//!
//! A mount is a translation of paths, not something the host filesystem knows
//! about, so reading the prefix's `/private/etc` shows the prefix's own entries
//! and not the host's `resolv.conf` that the guest can open there. Whoever reads
//! a directory for the guest asks here, once per directory, what to add and
//! what to leave out.

use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::mount::Table;

/// The changes one directory's listing needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    /// Names the directory's own entries must not show. Every mount point is
    /// here, listed or not: a listed one is shown from [`Self::shown`] instead,
    /// so the guest sees what it would open rather than what the mount covers.
    hidden: Vec<Vec<u8>>,
    shown: Vec<Shown>,
}

/// A mount point a listing shows after the directory's own entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    pub name: CString,
    pub ino: u64,
    /// A `DT_*` value.
    pub kind: u8,
}

impl Listing {
    /// Whether the directory's own entry `name` is left out.
    pub fn hides(&self, name: &[u8]) -> bool {
        self.hidden.iter().any(|hidden| hidden == name)
    }

    /// The mount points to show, in table order.
    pub fn shown(&self) -> &[Shown] {
        &self.shown
    }
}

/// The listing changes for the directory with this device and inode, or `None`
/// if it has none - which is every directory in identity mode.
///
/// Matched by identity rather than by path because the guest reaches a
/// directory through symlinks (`/etc` is `private/etc`), relative paths and
/// descriptors, and only the inode is the same along all of them.
pub fn listing(dev: u64, ino: u64) -> Option<Listing> {
    let table = super::table();
    if table.is_identity() {
        return None;
    }
    build(table, dev, ino)
}

/// [`listing`] for the directory at host path `directory`.
pub fn listing_of(directory: &Path) -> Option<Listing> {
    let table = super::table();
    if table.is_identity() {
        return None;
    }
    let metadata = std::fs::metadata(directory).ok()?;
    build(table, metadata.dev(), metadata.ino())
}

fn build(table: &Table, dev: u64, ino: u64) -> Option<Listing> {
    let mut listing = Listing::default();
    for point in table.mount_points() {
        let Some(parent) = table.to_host(point.parent) else {
            continue;
        };
        let Ok(parent) = std::fs::metadata(parent) else {
            continue;
        };
        if (parent.dev(), parent.ino()) != (dev, ino) {
            continue;
        }
        listing.hidden.push(point.name.as_bytes().to_vec());
        if !point.listed {
            continue;
        }
        // Followed, as opening it would be. A target that is not there is not
        // shown: the guest would be told `ENOENT` for the name anyway.
        let Ok(target) = std::fs::metadata(point.host) else {
            continue;
        };
        let Ok(name) = CString::new(OsStr::new(point.name).as_bytes()) else {
            continue;
        };
        listing.shown.push(Shown {
            name,
            ino: target.ino(),
            kind: kind(target.file_type()),
        });
    }
    (!listing.hidden.is_empty()).then_some(listing)
}

/// The `DT_*` value for a file type.
fn kind(file: std::fs::FileType) -> u8 {
    use std::os::unix::fs::FileTypeExt;
    match file {
        f if f.is_dir() => libc::DT_DIR,
        f if f.is_file() => libc::DT_REG,
        f if f.is_symlink() => libc::DT_LNK,
        f if f.is_char_device() => libc::DT_CHR,
        f if f.is_block_device() => libc::DT_BLK,
        f if f.is_fifo() => libc::DT_FIFO,
        f if f.is_socket() => libc::DT_SOCK,
        _ => libc::DT_UNKNOWN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A prefix root with the directories the mounts sit in, named per test
    /// so the suite can run in parallel.
    fn prefix(test: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("r3-listing-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["private/etc", "Volumes/Host", "Users"] {
            std::fs::create_dir_all(root.join(dir)).expect("a fixture directory");
        }
        root
    }

    fn of(table: &Table, host: &Path) -> Option<Listing> {
        let metadata = std::fs::metadata(host).expect("the listed directory");
        build(table, metadata.dev(), metadata.ino())
    }

    #[test]
    fn the_root_shows_dev_from_the_host() {
        let root = prefix("dev");
        let table = Table::rooted(&root, "user", None);
        let listing = of(&table, &root).expect("the root has a mount point");
        assert!(listing.hides(b"dev"));
        let dev = std::fs::metadata("/dev").expect("/dev");
        assert_eq!(
            listing.shown(),
            [Shown {
                name: c"dev".to_owned(),
                ino: dev.ino(),
                kind: libc::DT_DIR,
            }]
        );
        std::fs::remove_dir_all(root).expect("the fixture is removed");
    }

    /// The prefix's own placeholder is left out, and nothing is shown in its
    /// place: the escape hatch is reachable only by name.
    #[test]
    fn volumes_hides_the_host_mount() {
        let root = prefix("volumes");
        let table = Table::rooted(&root, "user", None);
        let listing = of(&table, &root.join("Volumes")).expect("a hidden mount point");
        assert!(listing.hides(b"Host"));
        assert!(listing.shown().is_empty());
        std::fs::remove_dir_all(root).expect("the fixture is removed");
    }

    /// Reached through the `/etc` symlink, the directory is still the one the
    /// file mount sits in.
    #[test]
    fn a_file_mount_is_shown_through_a_symlinked_parent() {
        let root = prefix("etc");
        std::os::unix::fs::symlink("private/etc", root.join("etc")).expect("a symlink");
        let host = tempfile_target(&root);
        let table = table_with_file(&root, &host);
        let listing = of(&table, &root.join("etc")).expect("a file mount point");
        assert_eq!(listing.shown().len(), 1);
        assert_eq!(listing.shown()[0].name.as_bytes(), b"resolv.conf");
        assert_eq!(listing.shown()[0].kind, libc::DT_REG);
        std::fs::remove_dir_all(root).expect("the fixture is removed");
    }

    #[test]
    fn a_directory_with_no_mount_point_is_left_alone() {
        let root = prefix("none");
        let table = Table::rooted(&root, "user", None);
        assert_eq!(of(&table, &root.join("Users")), None);
        std::fs::remove_dir_all(root).expect("the fixture is removed");
    }

    #[test]
    fn a_host_home_is_shown_in_users() {
        let root = prefix("home");
        let home = root.join("elsewhere");
        std::fs::create_dir_all(&home).expect("a home");
        let table = Table::rooted(&root, "someone", Some(&home));
        let listing = of(&table, &root.join("Users")).expect("the home mount point");
        assert_eq!(listing.shown()[0].name.as_bytes(), b"someone");
        assert_eq!(listing.shown()[0].kind, libc::DT_DIR);
        std::fs::remove_dir_all(root).expect("the fixture is removed");
    }

    /// The host's `/etc/resolv.conf` is not guaranteed on a build machine, so
    /// the file test checks the same shape against a file it made: the table's
    /// own `resolv.conf` mount is checked by `mount`'s tests.
    fn tempfile_target(root: &Path) -> PathBuf {
        let file = root.join("host-resolv.conf");
        std::fs::write(&file, b"nameserver 127.0.0.1\n").expect("a file");
        file
    }

    fn table_with_file(root: &Path, file: &Path) -> Table {
        Table::with(
            root,
            [("/private/etc/resolv.conf", file.to_path_buf(), true)],
        )
    }
}
