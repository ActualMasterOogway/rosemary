//! Making the guest's `.app` the process's main bundle.
//!
//! ## The problem, which nothing in the loader can fix
//!
//! Everything else this runtime does is a question about the *guest image*:
//! where its segments go, what its imports resolve to, how its calls are
//! marshalled. This is a question about the **process**, and the process is
//! ours.
//!
//! `[NSBundle mainBundle]` is derived from the running executable's path — walk
//! up from `<X>.app/Contents/MacOS/<exe>` and you have `<X>.app`. Under this
//! runtime the running executable is `rosetta3`, wherever it was built, so
//! `mainBundle` is not an application bundle at all. `NSApplicationMain` then
//! finds no `Info.plist`, loads no `NSMainNibFile`, creates no window and no
//! delegate, and the program sits in `[NSApp run]` forever having done nothing.
//! No diagnostic anywhere: an application with no nib is a legal application.
//!
//! Everything downstream inherits it. `[[NSBundle mainBundle] pathForResource:]`
//! is how a bundled program finds its own data — a GameMaker runner loads
//! `game.ios` that way — and `@executable_path` in a bundled dylib's install
//! name resolves against the same directory.
//!
//! ## What is done about it
//!
//! The process re-executes itself from inside a *mirror* of the guest's bundle:
//! a directory whose `Contents/Info.plist` is the guest's, whose `Resources`
//! and every other `Contents` entry are symlinks to the guest's, and whose
//! `Contents/MacOS/<exe>` is a copy of this binary. From there `mainBundle` is
//! the guest's bundle in every respect a program can observe, because it *is*
//! the guest's bundle everywhere except the one file that has to be arm64.
//!
//! Three details, each of which was a failure before it was a line of code:
//!
//! - **`Info.plist` is copied, not symlinked.** `CFBundle` follows the symlink
//!   happily; `codesign` and the kernel's signature check do not, and a bundle
//!   whose `Info.plist` is a symlink cannot be signed at all.
//! - **The executable is written to a fresh path, never over an existing one.**
//!   Overwriting a Mach-O that the kernel has already seen leaves a signature
//!   that no longer matches its contents, and the *next* execution dies with
//!   `SIGKILL` and no message whatsoever. Writing to a temporary name and
//!   renaming it into place gives the kernel a file it has never validated.
//! - **The mirror is keyed by the host binary's identity**, so a rebuilt
//!   `rosetta3` does not reuse the previous copy.
//!
//! ## What this is not
//!
//! Not a sandbox, not an installer, and not a modification of the guest's
//! bundle: nothing is ever written inside the application. The mirror lives in
//! this user's cache directory and is disposable — deleting it costs one file
//! copy on the next launch.
//!
//! A guest that is not inside a `.app` needs none of this and gets none of it:
//! [`remirror`] answers `None` and the driver runs the program directly, which
//! is every test fixture in this repository and every command-line binary.

use std::path::{Path, PathBuf};

/// Set in the re-executed process, so it never re-executes again.
///
/// The guard is an environment variable rather than a check on our own path
/// because the check would have to be "am I already inside a bundle", which is
/// true for a `rosetta3` that someone installed into one.
const GUARD: &str = "ROSETTA3_BUNDLED";

/// The bundle a path is the main executable of: `<X>.app` for
/// `<X>.app/Contents/MacOS/<exe>`.
///
/// Shape rather than name: `CFBundle`'s own rule is the directory two levels
/// above an executable in `Contents/MacOS`, and it does not care what the
/// executable is called or whether it matches `CFBundleExecutable`.
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

/// Build a mirror of the guest's bundle around this executable and return the
/// path to run instead of the current one.
///
/// `None` when there is nothing to do — the guest is not in a bundle, this
/// process is already the re-executed one, or the mirror could not be built.
/// The last of those is deliberately not an error: a program that does not
/// consult its bundle runs perfectly without one, and refusing to start would
/// turn a cosmetic problem into a fatal one.
pub fn remirror(guest: &Path) -> Option<PathBuf> {
    if std::env::var_os(GUARD).is_some() {
        return None;
    }
    // Canonical first: a guest launched as `./Mac_Runner` from inside its own
    // `Contents/MacOS` has `.` for a parent, and the shape test would say it is
    // not in a bundle — which is how a program launched the most natural way of
    // all would have been the one case this never fired for.
    let guest = std::fs::canonicalize(guest).ok()?;
    let app = enclosing(&guest)?;
    let host = std::env::current_exe().ok()?;
    let mirror = cache_dir()?.join(key(&app, &host)?).join(app.file_name()?);
    let exe_name = guest.file_name()?;

    let contents = mirror.join("Contents");
    let macos = contents.join("MacOS");
    std::fs::create_dir_all(&macos).ok()?;

    // `Info.plist` is a real file; see the module note on why it cannot be a
    // symlink. Everything else in `Contents` is the guest's own, reached
    // through a link, so a bundle with a gigabyte of resources costs nothing.
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
///
/// Returns only on failure: a successful `execv` does not come back. The
/// argument vector is this process's own, so the re-executed copy sees exactly
/// the command line the user typed — including the guest path, which is still
/// the *real* one and not the mirror's, so nothing about loading changes.
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
    Some(PathBuf::from(home).join("Library/Caches/rosetta3/bundles"))
}

/// A directory name that changes when either the application or this binary
/// does, so a rebuilt `rosetta3` never runs from a stale copy of itself.
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
/// Best-effort throughout: a link that already exists is left alone, and one
/// that cannot be made is skipped. A missing resource is the guest's problem to
/// report in its own words, and a hard failure here would turn "this program
/// cannot find its icon" into "this program will not start".
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
        let dir = std::env::temp_dir().join(format!("rosetta3-bundle-{}", std::process::id()));
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
