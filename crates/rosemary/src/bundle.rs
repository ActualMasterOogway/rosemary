use std::path::{Path, PathBuf};

/// Set in the re-executed process, so it never re-executes again.
const GUARD: &str = "ROSEMARY_BUNDLED";

/// Resolve a bundle to its executable without resolving the bundle's symlinks.
/// Loose executable paths pass through unchanged.
pub fn executable(path: &Path) -> std::io::Result<PathBuf> {
    if !path.is_dir() {
        return Ok(path.to_path_buf());
    }
    let invalid = |message| std::io::Error::new(std::io::ErrorKind::InvalidData, message);
    let plist_path = path.join("Contents/Info.plist");
    let bytes = std::fs::read(&plist_path)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", plist_path.display())))?;
    let info = foundation::plist::parse(&bytes)
        .map_err(|e| invalid(format!("{}: {e}", plist_path.display())))?;
    if info.as_dictionary().is_none() {
        return Err(invalid("Info.plist must contain a dictionary".to_string()));
    }
    // Match CFBundle's fallback for an absent or empty executable name.
    let name = ["CFBundleExecutable", "NSExecutable"]
        .iter()
        .find_map(|key| info.get(key)?.as_str().filter(|name| !name.is_empty()))
        .map(std::ffi::OsStr::new)
        .or_else(|| path.file_stem())
        .ok_or_else(|| invalid("the bundle has no executable name".to_string()))?;
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(invalid(
            "the bundle executable name must be a filename".to_string(),
        ));
    }
    let exe = path.join("Contents/MacOS").join(name);
    let metadata = std::fs::metadata(&exe)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", exe.display())))?;
    if !metadata.is_file() {
        return Err(invalid(format!("{} is not a file", exe.display())));
    }
    Ok(exe)
}

/// The bundle a path is the main executable of: `<X>.app` for
/// `<X>.app/Contents/MacOS/<exe>`.
pub fn enclosing(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let app = contents.parent()?;
    contents
        .join("Info.plist")
        .is_file()
        .then(|| app.to_path_buf())
}

/// Build a mirror of the guest's bundle around this executable and return the path to
/// run instead of the current one.
pub fn remirror(guest: &Path) -> Option<PathBuf> {
    if std::env::var_os(GUARD).is_some() {
        return None;
    }
    // Canonical first.
    let guest = std::fs::canonicalize(guest).ok()?;
    let app = enclosing(&guest)?;
    let host = std::env::current_exe().ok()?;
    // The mirror reproduces the **shape** of the original tree, not just the
    // `.app`: two levels of ancestor, each with its siblings linked in.
    let root = cache_dir()?.join(key(&app, &host)?);
    let mirror = link_ancestors(&app, &root)?;
    let exe_name = guest.file_name()?;

    let contents = mirror.join("Contents");
    let macos = contents.join("MacOS");
    std::fs::create_dir_all(&macos).ok()?;

    // `Info.plist` is a real file; see the module note on why it cannot be a
    // symlink.
    let plist = app.join("Contents/Info.plist");
    copy_if_newer(&plist, &contents.join("Info.plist"))?;
    link_children(&app.join("Contents"), &contents, &["Info.plist", "MacOS"]);
    // …including the guest's own `MacOS` directory, minus the executable
    // itself, so a bundled dylib the guest `dlopen`s by bundle path is found.
    link_children(&app.join("Contents/MacOS"), &macos, &[]);

    let dest = macos.join(exe_name);
    // Only remove a link we made; a stale *copy* is replaced by the rename
    // below, which is the whole point of writing to a temporary name.
    let _ = std::fs::remove_file(&dest);
    let staging = macos.join(format!(".{}.staging", exe_name.to_string_lossy()));
    std::fs::copy(&host, &staging).ok()?;
    std::fs::rename(&staging, &dest).ok()?;

    Some(dest)
}

/// Replace this process with `exe`, keeping the arguments.
pub fn reexec(exe: &Path) -> std::io::Error {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // SAFETY: `setenv` before any thread has been started; the strings are
    // NUL-terminated and copied by the call.
    unsafe { std::env::set_var(GUARD, "1") };

    let Ok(path) = CString::new(exe.as_os_str().as_bytes()) else {
        return std::io::Error::other("the mirror's path contains a NUL");
    };
    let args: Vec<CString> = std::env::args_os()
        .filter_map(|a| CString::new(a.as_bytes()).ok())
        .collect();
    let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    // SAFETY: `path` and every element of `argv` are NUL-terminated and live
    // until the call returns, which it only does on failure.
    unsafe { libc::execv(path.as_ptr(), argv.as_ptr()) };
    std::io::Error::last_os_error()
}

/// Where mirrors live: this user's cache, never inside the application.
fn cache_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Caches/rosemary/bundles"))
}

/// A directory name that changes when either the application or this binary
/// does, so a rebuilt `rosemary` never runs from a stale copy of itself.
fn key(app: &Path, host: &Path) -> Option<String> {
    let stamp = |p: &Path| -> Option<u128> {
        let m = std::fs::metadata(p).ok()?;
        let t = m.modified().ok()?;
        Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos())
    };
    // Not cryptographic and does not need to be: this distinguishes one
    // developer's builds from each other, and a collision costs a re-copy.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in app.as_os_str().as_encoded_bytes() {
        h = (h ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
    }
    h = h.wrapping_mul(31).wrapping_add(stamp(host)? as u64);
    Some(format!("{h:016x}"))
}

/// Symlink every entry of `from` into `to`, skipping `except`.
///
/// Best-effort throughout.
fn link_ancestors(app: &Path, root: &Path) -> Option<PathBuf> {
    let app_name = app.file_name()?;
    let Some(parent) = app.parent() else {
        std::fs::create_dir_all(root).ok()?;
        return Some(root.join(app_name));
    };
    let parent_name = parent.file_name()?;
    let mirror_parent = root.join(parent_name);
    std::fs::create_dir_all(&mirror_parent).ok()?;

    // The grandparent's children beside the reproduced parent, then the
    // parent's beside the reproduced `.app`.
    if let Some(grand) = parent.parent() {
        link_children(grand, root, &[&parent_name.to_string_lossy()]);
    }
    link_children(parent, &mirror_parent, &[&app_name.to_string_lossy()]);
    Some(mirror_parent.join(app_name))
}

fn link_children(from: &Path, to: &Path, except: &[&str]) {
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if except.iter().any(|e| name == **e) {
            continue;
        }
        let dest = to.join(&name);
        if std::fs::symlink_metadata(&dest).is_ok() {
            continue;
        }
        let _ = std::os::unix::fs::symlink(entry.path(), dest);
    }
}

/// Copy `from` to `to` unless `to` is already at least as new.
fn copy_if_newer(from: &Path, to: &Path) -> Option<()> {
    let src = std::fs::metadata(from).ok()?.modified().ok()?;
    let fresh = std::fs::metadata(to)
        .and_then(|m| m.modified())
        .map(|dst| dst >= src)
        .unwrap_or(false);
    if !fresh {
        std::fs::copy(from, to).ok()?;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_bundle_executables_from_xml_and_binary_plists() {
        let dir = std::env::temp_dir().join(format!("rosemary-resolve-{}", std::process::id()));
        let app = dir.join("Different Name.app");
        let macos = app.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        let exe = macos.join("Runner");
        std::fs::write(&exe, b"").unwrap();
        let plist = app.join("Contents/Info.plist");
        let binary: &[u8] = &[
            98, 112, 108, 105, 115, 116, 48, 48, 209, 1, 2, 95, 16, 18, 67, 70, 66, 117, 110, 100,
            108, 101, 69, 120, 101, 99, 117, 116, 97, 98, 108, 101, 86, 82, 117, 110, 110, 101,
            114, 8, 11, 32, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 39,
        ];
        for info in [
            &b"<plist><dict><key>CFBundleExecutable</key><string>Runner</string></dict></plist>"[..],
            binary,
        ] {
            std::fs::write(&plist, info).unwrap();
            assert_eq!(executable(&app).unwrap(), exe);
            assert_eq!(executable(&exe).unwrap(), exe);
            let link = dir.join("Linked.app");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&app, &link).unwrap();
            assert_eq!(
                executable(&link).unwrap(),
                link.join("Contents/MacOS/Runner")
            );
        }
        std::fs::write(macos.join("Different Name"), b"").unwrap();
        for info in [
            "<dict></dict>",
            "<dict><key>CFBundleExecutable</key><string></string></dict>",
        ] {
            std::fs::write(&plist, format!("<plist>{info}</plist>")).unwrap();
            assert_eq!(executable(&app).unwrap(), macos.join("Different Name"));
        }
        std::fs::write(
            &plist,
            "<plist><dict><key>NSExecutable</key><string>Runner</string></dict></plist>",
        )
        .unwrap();
        assert_eq!(executable(&app).unwrap(), exe);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_bundles_report_the_problem() {
        let app = std::env::temp_dir().join(format!("rosemary-invalid-{}.app", std::process::id()));
        std::fs::create_dir_all(app.join("Contents/MacOS/Directory")).unwrap();
        let plist = app.join("Contents/Info.plist");
        assert!(
            executable(&app)
                .unwrap_err()
                .to_string()
                .contains("Info.plist")
        );
        for info in ["garbage", "<plist><array></array></plist>"] {
            std::fs::write(&plist, info).unwrap();
            assert_eq!(
                executable(&app).unwrap_err().kind(),
                std::io::ErrorKind::InvalidData
            );
        }
        for (name, message) in [
            ("Missing", "Missing"),
            ("Directory", "not a file"),
            ("../outside", "must be a filename"),
            ("/bin/sh", "must be a filename"),
        ] {
            std::fs::write(&plist, format!("<plist><dict><key>CFBundleExecutable</key><string>{name}</string></dict></plist>")).unwrap();
            let error = executable(&app).unwrap_err().to_string();
            assert!(error.contains(message), "{error}");
        }
        std::fs::remove_dir_all(app).unwrap();
    }

    #[test]
    fn a_bundle_is_recognised_by_shape_and_not_by_name() {
        let dir = std::env::temp_dir().join(format!("rosemary-bundle-{}", std::process::id()));
        let macos = dir.join("Thing.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).expect("create");
        let exe = macos.join("whatever");
        std::fs::write(&exe, b"").expect("write");

        // No `Info.plist`: `CFBundle` would not call this a bundle either, and
        // mirroring one would produce a bundle with no `CFBundleExecutable`.
        assert_eq!(enclosing(&exe), None);

        std::fs::write(dir.join("Thing.app/Contents/Info.plist"), b"").expect("write");
        assert_eq!(enclosing(&exe), Some(dir.join("Thing.app")));

        // The executable's name is not consulted, which matches `CFBundle`:
        // the rule is two directories up from `Contents/MacOS`.
        let other = macos.join("some-other-name");
        std::fs::write(&other, b"").expect("write");
        assert_eq!(enclosing(&other), Some(dir.join("Thing.app")));

        // A plain path is not in a bundle and must not be treated as one.
        assert_eq!(enclosing(Path::new("/bin/ls")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
