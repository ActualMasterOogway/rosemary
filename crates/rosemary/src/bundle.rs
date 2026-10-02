use std::path::{Path, PathBuf};

/// Set in the re-executed process, so it never re-executes again.
const GUARD: &str = "ROSEMARY_BUNDLED";

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
