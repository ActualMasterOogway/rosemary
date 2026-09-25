use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::format::manifest::{Kind, Manifest};
use crate::format::{Invalid, check_symlink_target, parent_of, split_path};
use crate::verify::host;

/// Unpack the gzip-compressed tar `artifact` into `dest`, which must not
/// exist, refusing anything the manifest does not name. Returns the number of
/// entries written.
pub fn extract(artifact: impl Read, dest: &Path, manifest: &Manifest) -> Result<usize, Invalid> {
    std::fs::DirBuilder::new()
        .mode(0o755)
        .create(dest)
        .map_err(|e| Invalid::new(format!("{}: {e}", dest.display())))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(artifact));
    let mut made: HashSet<String> = ["/".to_string()].into();
    let mut seen: HashSet<String> = HashSet::new();
    let mut modes: Vec<(std::path::PathBuf, u32)> = Vec::new();
    let mut count = 0;
    let entries = archive
        .entries()
        .map_err(|e| Invalid::new(format!("tarball: {e}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| Invalid::new(format!("tarball: {e}")))?;
        let path = tar_path(&entry.path_bytes())?;
        let header = entry.header();
        let kind = header.entry_type();
        if path == "/" {
            if !kind.is_dir() {
                return Err(Invalid::new("tarball: root entry is not a directory"));
            }
            continue;
        }
        if !seen.insert(path.clone()) {
            return Err(Invalid::new(format!("tarball: '{path}' appears twice")));
        }
        let Some(expected) = manifest.get(&path) else {
            return Err(Invalid::new(format!(
                "tarball: '{path}' is not in the manifest"
            )));
        };
        if !made.contains(parent_of(&path)) {
            return Err(Invalid::new(format!(
                "tarball: '{path}' comes before its parent directory"
            )));
        }
        let mode = header
            .mode()
            .map_err(|e| Invalid::new(format!("tarball: '{path}': {e}")))?;
        if mode & 0o7000 != 0 {
            return Err(Invalid::new(format!(
                "tarball: '{path}' is set-id or sticky"
            )));
        }
        let mode = mode & 0o777;
        let at = host(dest, &path);
        if kind.is_dir() {
            if !matches!(expected.kind, Kind::Directory { .. }) {
                return Err(wrong(&path, "a directory", &expected.kind));
            }
            // Owner-writable while its contents are made; given the mode the
            // archive asked for once they are.
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&at)
                .map_err(|e| Invalid::new(format!("{}: {e}", at.display())))?;
            modes.push((at, mode));
            made.insert(path);
        } else if kind.is_file() {
            let Kind::File { size, .. } = expected.kind else {
                return Err(wrong(&path, "a file", &expected.kind));
            };
            let declared = entry.size();
            if declared != size {
                return Err(Invalid::new(format!(
                    "tarball: '{path}' is {declared} bytes, manifest says {size}"
                )));
            }
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&at)
                .map_err(|e| Invalid::new(format!("{}: {e}", at.display())))?;
            // `take` as well as the header check: the reader is bounded by the
            // header already, and this makes the bound this module's own.
            let written = std::io::copy(&mut (&mut entry).take(size), &mut out)
                .map_err(|e| Invalid::new(format!("tarball: '{path}': {e}")))?;
            if written != size {
                return Err(Invalid::new(format!("tarball: '{path}' is truncated")));
            }
            out.set_permissions(std::fs::Permissions::from_mode(mode))
                .and_then(|()| out.flush())
                .map_err(|e| Invalid::new(format!("{}: {e}", at.display())))?;
        } else if kind.is_symlink() {
            if !matches!(expected.kind, Kind::Symlink { .. }) {
                return Err(wrong(&path, "a symlink", &expected.kind));
            }
            let target = entry
                .link_name_bytes()
                .ok_or_else(|| Invalid::new(format!("tarball: '{path}' has no link target")))?;
            let target = std::str::from_utf8(&target)
                .map_err(|_| Invalid::new(format!("tarball: '{path}' has a non-UTF-8 target")))?;
            check_symlink_target(&path, target, "tarball")?;
            std::os::unix::fs::symlink(target, &at)
                .map_err(|e| Invalid::new(format!("{}: {e}", at.display())))?;
        } else {
            return Err(Invalid::new(format!(
                "tarball: '{path}' is not a file, directory or symlink"
            )));
        }
        count += 1;
    }
    for (at, mode) in modes.iter().rev() {
        std::fs::set_permissions(at, std::fs::Permissions::from_mode(*mode))
            .map_err(|e| Invalid::new(format!("{}: {e}", at.display())))?;
    }
    Ok(count)
}

fn wrong(path: &str, found: &str, expected: &Kind) -> Invalid {
    Invalid::new(format!(
        "tarball: '{path}' is {found}, manifest says {}",
        expected.name()
    ))
}

/// A tar member's name as a manifest path: `./usr/lib` is `/usr/lib`, and `.`
/// is the root.
fn tar_path(name: &[u8]) -> Result<String, Invalid> {
    if name.contains(&0) {
        return Err(Invalid::new("tarball: entry name contains NUL"));
    }
    let name =
        std::str::from_utf8(name).map_err(|_| Invalid::new("tarball: entry name is not UTF-8"))?;
    if matches!(name, "." | "./") {
        return Ok("/".to_string());
    }
    let name = name.strip_prefix("./").unwrap_or(name);
    let name = name.trim_end_matches('/');
    if name.is_empty() || name.starts_with('/') {
        return Err(Invalid::new(format!(
            "tarball: entry '{name}' is absolute or empty"
        )));
    }
    let path = format!("/{name}");
    split_path(&path, "tarball")?;
    Ok(path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::format::manifest::tests::sample;
    use crate::verify::tests::scratch;
    use crate::verify::{Scope, compare};

    /// How one member of a hand-built archive is written.
    pub(crate) enum Member<'a> {
        Dir(&'a str, u32),
        File(&'a str, u32, &'a [u8]),
        /// A file whose header claims `size` but carries `content`.
        Lying(&'a str, u32, u64, &'a [u8]),
        Link(&'a str, &'a str),
        Hard(&'a str, &'a str),
        Fifo(&'a str),
    }

    /// A gzip-compressed tar built member by member, with headers written raw
    /// so that names the `tar` crate's builder would refuse can be tested.
    pub(crate) fn tarball(members: &[Member]) -> Vec<u8> {
        let mut raw = Vec::new();
        for member in members {
            let mut header = tar::Header::new_gnu();
            let (name, content): (&str, &[u8]) = match member {
                Member::Dir(name, mode) => {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_mode(*mode);
                    header.set_size(0);
                    (name, b"")
                }
                Member::File(name, mode, content) => {
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_mode(*mode);
                    header.set_size(content.len() as u64);
                    (name, content)
                }
                Member::Lying(name, mode, size, content) => {
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_mode(*mode);
                    header.set_size(*size);
                    (name, content)
                }
                Member::Link(name, target) | Member::Hard(name, target) => {
                    header.set_entry_type(match member {
                        Member::Link(..) => tar::EntryType::Symlink,
                        _ => tar::EntryType::Link,
                    });
                    header.set_mode(0o777);
                    header.set_size(0);
                    let bytes = target.as_bytes();
                    header.as_old_mut().linkname[..bytes.len()].copy_from_slice(bytes);
                    (name, b"")
                }
                Member::Fifo(name) => {
                    header.set_entry_type(tar::EntryType::Fifo);
                    header.set_mode(0o644);
                    header.set_size(0);
                    (name, b"")
                }
            };
            let bytes = name.as_bytes();
            header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
            header.set_cksum();
            raw.extend_from_slice(header.as_bytes());
            raw.extend_from_slice(content);
            raw.resize(raw.len().div_ceil(512) * 512, 0);
        }
        raw.extend_from_slice(&[0; 1024]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).unwrap();
        gz.finish().unwrap()
    }

    /// The sample manifest's tree as an archive, in canonical order.
    pub(crate) fn good_members() -> Vec<Member<'static>> {
        let (_, files) = sample();
        vec![
            Member::Dir("./", 0o755),
            Member::Dir("./usr/", 0o755),
            Member::Dir("./usr/lib/", 0o755),
            Member::File("./usr/lib/data.txt", 0o644, files[0].1),
            Member::File("./usr/lib/libA.dylib", 0o755, files[1].1),
            Member::Link("./usr/lib/libB.dylib", "libA.dylib"),
            Member::Dir("./usr/share/", 0o755),
            Member::Dir("./usr/share/doc/", 0o755),
            Member::File("./usr/share/doc/readme", 0o644, files[2].1),
        ]
    }

    #[test]
    fn a_good_archive_unpacks_to_a_verified_tree() {
        let (manifest, _) = sample();
        let at = scratch("unpack-good");
        let dest = at.join("root");
        let count = extract(&tarball(&good_members())[..], &dest, &manifest).expect("clean");
        assert_eq!(count, manifest.entries.len());
        assert_eq!(compare(&dest, &manifest, false, &Scope::All).unwrap(), []);
        let _ = std::fs::remove_dir_all(&at);
    }

    /// Every refusal on the list, each on an otherwise good archive.
    #[test]
    fn hostile_archives_are_refused() {
        let (manifest, files) = sample();
        let at = scratch("unpack-hostile");
        let mut cases: Vec<(&str, Vec<Member>)> = Vec::new();
        let with = |replace: usize, member: Member<'static>| {
            let mut members = good_members();
            members[replace] = member;
            members
        };
        let plus = |member: Member<'static>| {
            let mut members = good_members();
            members.push(member);
            members
        };
        cases.push(("absolute", plus(Member::File("/etc/evil", 0o644, b"x"))));
        cases.push((
            "dot-dot",
            plus(Member::File("./usr/../../evil", 0o644, b"x")),
        ));
        cases.push((
            "twice",
            plus(Member::File("./usr/lib/data.txt", 0o644, files[0].1)),
        ));
        cases.push((
            "unnamed",
            plus(Member::File("./usr/lib/extra", 0o644, b"x")),
        ));
        cases.push((
            "wrong type",
            with(4, Member::Link("./usr/lib/libA.dylib", "data.txt")),
        ));
        cases.push(("fifo", with(3, Member::Fifo("./usr/lib/data.txt"))));
        cases.push((
            "hard link",
            with(
                5,
                Member::Hard("./usr/lib/libB.dylib", "usr/lib/libA.dylib"),
            ),
        ));
        cases.push((
            "setuid",
            with(4, Member::File("./usr/lib/libA.dylib", 0o4755, files[1].1)),
        ));
        cases.push(("sticky", with(2, Member::Dir("./usr/lib/", 0o1755))));
        cases.push((
            "size",
            with(
                3,
                Member::File("./usr/lib/data.txt", 0o644, b"a much longer body\n"),
            ),
        ));
        cases.push((
            "bomb",
            with(
                3,
                Member::Lying("./usr/lib/data.txt", 0o644, 1 << 40, b"data\n"),
            ),
        ));
        cases.push((
            "escaping link",
            with(
                5,
                Member::Link("./usr/lib/libB.dylib", "../../../etc/passwd"),
            ),
        ));
        cases.push((
            "absolute link",
            with(5, Member::Link("./usr/lib/libB.dylib", "/etc/passwd")),
        ));
        // A file whose parent never appeared: had `usr/lib` been a symlink,
        // this is the write that would have gone through it.
        let mut orphan = good_members();
        orphan.remove(2);
        cases.push(("parent not made", orphan));
        let mut root_file = good_members();
        root_file[0] = Member::File("./", 0o644, b"");
        cases.push(("root is a file", root_file));

        for (index, (name, members)) in cases.iter().enumerate() {
            let dest = at.join(format!("case{index}"));
            let result = extract(&tarball(members)[..], &dest, &manifest);
            assert!(result.is_err(), "{name}: was accepted");
            // Nothing escaped the destination.
            assert!(!Path::new("/etc/evil").exists() || name != &"absolute");
            assert!(!at.join("evil").exists(), "{name}: wrote outside");
        }
        let _ = std::fs::remove_dir_all(&at);
    }

    /// Extraction refuses to write through a symlink even if one is already
    /// sitting in the destination: every file is created `O_EXCL|O_NOFOLLOW`.
    #[test]
    fn a_name_is_never_written_through() {
        let (manifest, _) = sample();
        let at = scratch("unpack-through");
        let outside = at.join("outside");
        std::fs::write(&outside, b"precious").unwrap();
        // Destination must not exist; if it does, nothing is written.
        let dest = at.join("root");
        std::fs::create_dir(&dest).unwrap();
        assert!(extract(&tarball(&good_members())[..], &dest, &manifest).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"precious");
        let _ = std::fs::remove_dir_all(&at);
    }

    #[test]
    fn member_names_are_normalised_or_refused() {
        assert_eq!(tar_path(b"./").unwrap(), "/");
        assert_eq!(tar_path(b"./usr/lib/").unwrap(), "/usr/lib");
        assert_eq!(tar_path(b"usr").unwrap(), "/usr");
        for bad in [&b"/usr"[..], b"../x", b"./a/../b", b"a\0b", b"./a//b", b""] {
            assert!(tar_path(bad).is_err(), "{bad:?}");
        }
    }
}
