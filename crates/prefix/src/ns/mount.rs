use std::path::{Component, Path, PathBuf};

/// One guest directory and the host directory it is.
#[derive(Debug, Clone)]
pub struct Mount {
    /// An absolute, normalised guest path, without a trailing separator except
    /// for the root mount, which is exactly `/`.
    guest: String,
    host: PathBuf,
}

/// The mounts, ordered so that the first match is the longest one.
///
/// Two orders are kept rather than one sorted list searched twice, because the
/// longest *guest* path and the longest *host* path are not the same mount:
/// `/Volumes/Host` is a long guest path onto the shortest host path there is.
#[derive(Debug, Clone, Default)]
pub struct Table {
    by_guest: Vec<Mount>,
    by_host: Vec<Mount>,
}

impl Table {
    /// The empty table: a guest path is a host path.
    pub fn identity() -> Table {
        Table::default()
    }

    /// The table for a prefix whose guest `/` is `root`.
    ///
    /// `host_home` mounts the guest's `/Users/<user>` onto a host directory
    /// instead of the prefix's own; `None` keeps the prefix self-contained.
    /// Passed in rather than read from `$HOME` here, because a process-wide
    /// table is built once and a test that set the variable could not observe
    /// it.
    pub fn rooted(root: &Path, user: &str, host_home: Option<&Path>) -> Table {
        let mut mounts = vec![
            Mount {
                guest: "/".to_string(),
                host: root.to_path_buf(),
            },
            // Darwin's `/tmp` is a symlink to `/private/tmp`, and software
            // that resolves one and compares it against the other has to get
            // the same answer, so both name one directory.
            Mount {
                guest: "/tmp".to_string(),
                host: root.join("private/tmp"),
            },
            // The guest needs this host's real character devices: `/dev/null`,
            // `/dev/urandom` and the rest are the kernel's, not files a rootfs
            // could carry.
            Mount {
                guest: "/dev".to_string(),
                host: PathBuf::from("/dev"),
            },
            // The escape hatch, and the reason `to_guest` is total: every host
            // path has a guest spelling under here, which is how a guest
            // launched from outside the prefix can name its own bundle.
            Mount {
                guest: "/Volumes/Host".to_string(),
                host: PathBuf::from("/"),
            },
        ];
        if let Some(host_home) = host_home {
            mounts.push(Mount {
                guest: format!("/Users/{user}"),
                host: host_home.to_path_buf(),
            });
        }
        Table::new(mounts)
    }

    fn new(mounts: Vec<Mount>) -> Table {
        let mut by_guest = mounts.clone();
        by_guest.sort_by_key(|m| std::cmp::Reverse(m.guest.len()));
        let mut by_host = mounts;
        by_host.sort_by_key(|m| std::cmp::Reverse(m.host.as_os_str().len()));
        Table { by_guest, by_host }
    }

    pub fn is_identity(&self) -> bool {
        self.by_guest.is_empty()
    }

    /// The host path `guest` names, or `None` if no mount claims it.
    ///
    /// `None` for a relative path: it is resolved against the working
    /// directory, which is already a host directory inside the prefix, so
    /// rewriting it would apply the prefix twice.
    pub fn to_host(&self, guest: &str) -> Option<PathBuf> {
        let normalised = normalise(guest)?;
        let mount = self
            .by_guest
            .iter()
            .find(|m| under(&normalised, &m.guest))?;
        Some(join(
            &mount.host,
            &normalised[trimmed(&mount.guest).len()..],
        ))
    }

    /// The guest path that names `host`, or `None` if no mount reaches it.
    ///
    /// `None` is a refusal, not a fallback: a host path no mount reaches is one
    /// the guest was never supposed to see, and handing it over unchanged would
    /// be the leak this table is here to prevent.
    pub fn to_guest(&self, host: &Path) -> Option<PathBuf> {
        let host = host.to_str()?;
        let normalised = normalise(host)?;
        let mount = self
            .by_host
            .iter()
            .find(|m| m.host.to_str().is_some_and(|root| under(&normalised, root)))?;
        let root = mount.host.to_str()?;
        let rest = &normalised[trimmed(root).len()..];
        Some(join(Path::new(&mount.guest), rest))
    }
}

/// `path` lexically normalised: absolute, no `.`, no repeated or trailing
/// separators, `None` for anything else.
///
/// `..` is *not* resolved away. A guest's `..` means what the filesystem says
/// it means once symlinks are involved, and a table that collapsed it lexically
/// would answer about a different directory than the kernel would.
fn normalise(path: &str) -> Option<String> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return None;
    }
    let mut out = String::new();
    for part in path.components() {
        match part {
            Component::RootDir | Component::CurDir => continue,
            Component::Prefix(_) => return None,
            Component::ParentDir => {
                out.push('/');
                out.push_str("..");
            }
            Component::Normal(name) => {
                out.push('/');
                out.push_str(name.to_str()?);
            }
        }
    }
    Some(match out.is_empty() {
        true => "/".to_string(),
        false => out,
    })
}

/// Whether the normalised `path` is `root` or something inside it.
///
/// Whole components only, so `/dev` does not claim `/device`.
fn under(path: &str, root: &str) -> bool {
    let root = trimmed(root);
    if root.is_empty() {
        return path.starts_with('/');
    }
    path == root || (path.starts_with(root) && path.as_bytes().get(root.len()) == Some(&b'/'))
}

/// `root` without the trailing separator a root path has and nothing else does.
fn trimmed(root: &str) -> &str {
    match root {
        "/" => "",
        other => other.trim_end_matches('/'),
    }
}

/// `root` with `rest`, which is either empty or begins with a separator.
fn join(root: &Path, rest: &str) -> PathBuf {
    match rest.strip_prefix('/') {
        Some(relative) if !relative.is_empty() => root.join(relative),
        _ => root.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Table {
        Table::rooted(Path::new("/prefixes/default/root"), "user", None)
    }

    #[test]
    fn an_identity_table_claims_nothing() {
        let identity = Table::identity();
        assert!(identity.is_identity());
        assert_eq!(identity.to_host("/usr/lib/libSystem.B.dylib"), None);
        assert_eq!(identity.to_guest(Path::new("/usr/lib")), None);
    }

    #[test]
    fn a_darwin_path_lands_under_the_prefix_root() {
        assert_eq!(
            table().to_host("/usr/lib/libSystem.B.dylib"),
            Some(PathBuf::from(
                "/prefixes/default/root/usr/lib/libSystem.B.dylib"
            ))
        );
        assert_eq!(
            table().to_host("/"),
            Some(PathBuf::from("/prefixes/default/root"))
        );
    }

    /// The longest guest match wins, not the first one listed.
    #[test]
    fn a_longer_mount_beats_the_root() {
        assert_eq!(
            table().to_host("/tmp/x"),
            Some(PathBuf::from("/prefixes/default/root/private/tmp/x"))
        );
        assert_eq!(
            table().to_host("/dev/urandom"),
            Some(PathBuf::from("/dev/urandom"))
        );
    }

    /// A mount claims whole components. This is the bug a `starts_with` would
    /// have: `/dev` would swallow `/deuserper`.
    #[test]
    fn a_mount_does_not_claim_a_longer_name() {
        assert_eq!(
            table().to_host("/deuserper/x"),
            Some(PathBuf::from("/prefixes/default/root/deuserper/x"))
        );
    }

    /// Every host path has a guest spelling, which is what makes `to_guest`
    /// total - and what lets a guest run from outside its own prefix.
    #[test]
    fn a_host_path_outside_the_prefix_is_named_under_volumes() {
        assert_eq!(
            table().to_guest(Path::new("/home/user/Foo.app/Contents/MacOS/Foo")),
            Some(PathBuf::from(
                "/Volumes/Host/home/user/Foo.app/Contents/MacOS/Foo"
            ))
        );
        assert_eq!(
            table().to_host("/Volumes/Host/home/user/Foo.app"),
            Some(PathBuf::from("/home/user/Foo.app"))
        );
    }

    /// The prefix root is a longer *host* path than `/`, so it answers first
    /// and a file in the prefix is never named through the escape hatch.
    #[test]
    fn a_path_inside_the_prefix_is_named_by_its_darwin_path() {
        assert_eq!(
            table().to_guest(Path::new("/prefixes/default/root/usr/lib")),
            Some(PathBuf::from("/usr/lib"))
        );
        assert_eq!(
            table().to_guest(Path::new("/prefixes/default/root/private/tmp/x")),
            Some(PathBuf::from("/tmp/x"))
        );
    }

    #[test]
    fn every_mount_round_trips() {
        let table = table();
        for guest in [
            "/usr/lib/libSystem.B.dylib",
            "/tmp/x",
            "/dev/null",
            "/Volumes/Host/etc/hosts",
            "/Applications",
        ] {
            let host = table.to_host(guest).expect("a host path");
            assert_eq!(
                table.to_guest(&host).as_deref(),
                Some(Path::new(guest)),
                "{guest}"
            );
        }
    }

    /// What `-bundlePath` and the rest of `NSBundle` depend on: a guest joins
    /// onto the path it was given to find its own files, so the spelling it is
    /// given has to be one that maps back to the same place.
    #[test]
    fn a_bundles_paths_survive_being_joined_onto() {
        let table = table();
        let host = Path::new("/home/user/Games/Foo.app");
        let guest = table.to_guest(host).expect("a guest spelling");
        assert_eq!(
            guest,
            PathBuf::from("/Volumes/Host/home/user/Games/Foo.app")
        );
        // The guest joins onto what it was given, and lands back on the file.
        assert_eq!(
            table.to_host(guest.join("Contents/Resources/game.ios").to_str().unwrap()),
            Some(PathBuf::from(
                "/home/user/Games/Foo.app/Contents/Resources/game.ios"
            ))
        );
    }

    /// A relative path is already resolved against a host working directory, so
    /// applying the prefix to it would apply it twice.
    #[test]
    fn a_relative_path_is_left_alone() {
        assert_eq!(table().to_host("usr/lib"), None);
        assert_eq!(table().to_host(""), None);
    }

    /// Lexical `..` collapsing would answer about a different directory than
    /// the kernel does once a symlink is in the way, so it is carried through.
    #[test]
    fn a_parent_component_is_carried_not_collapsed() {
        assert_eq!(
            table().to_host("/usr/lib/../lib"),
            Some(PathBuf::from("/prefixes/default/root/usr/lib/../lib"))
        );
    }

    #[test]
    fn a_host_home_mount_replaces_the_prefixs_own() {
        let table = Table::rooted(
            Path::new("/prefixes/default/root"),
            "user",
            Some(Path::new("/home/user")),
        );
        assert_eq!(
            table.to_host("/Users/user/Documents"),
            Some(PathBuf::from("/home/user/Documents"))
        );
        // …and another user's home is still the prefix's.
        assert_eq!(
            table.to_host("/Users/someone/Documents"),
            Some(PathBuf::from(
                "/prefixes/default/root/Users/someone/Documents"
            ))
        );
    }
}
