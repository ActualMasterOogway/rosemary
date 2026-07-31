//! `@executable_path`, `@loader_path`, and `@rpath` expansion.
//!
//! A pure string algorithm with no I/O, in its own module for one reason: it is
//! the single most common cause of "why won't this application load", and a
//! function that can be exhaustively tested from a table is worth far more than
//! the same logic threaded through a loader that needs a filesystem to
//! exercise.
//!
//! The three prefixes answer three different questions:
//!
//! - **`@executable_path`** — relative to the *main executable*, no matter
//!   which image records the dependency. An app bundle's frameworks are found
//!   this way, and the answer is the same for every image in the process.
//! - **`@loader_path`** — relative to the image that *records* the dependency.
//!   A framework that ships its own private dylibs uses this, and the answer
//!   differs per image, which is exactly why expansion cannot be done once up
//!   front.
//! - **`@rpath`** — not a path at all but a *search*: substitute each entry of
//!   the `LC_RPATH` stack in turn and try them in order. An `LC_RPATH` entry may
//!   itself begin with `@executable_path` or `@loader_path`, so expansion is
//!   two levels deep.
//!
//! ## What this deliberately does not model
//!
//! `DYLD_LIBRARY_PATH`, `DYLD_FRAMEWORK_PATH`, and the rest of the `DYLD_*`
//! override family. They exist to let a *user* redirect a process's libraries,
//! and a compatibility layer that honoured the host's copy of them would let
//! the host environment reach into guest linking in ways nothing here models.
//! An embedder that wants them can prepend candidates itself.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Guard against an `LC_RPATH` stack that expands into an absurd search.
const MAX_CANDIDATES: usize = 4096;

/// Prefix marking a path relative to the main executable.
pub const EXECUTABLE_PATH: &str = "@executable_path";
/// Prefix marking a path relative to the image recording the dependency.
pub const LOADER_PATH: &str = "@loader_path";
/// Prefix marking a search through the `LC_RPATH` stack.
pub const RPATH: &str = "@rpath";

/// Everything an install name needs in order to become a set of real paths.
#[derive(Copy, Clone, Debug, Default)]
pub struct PathContext<'a> {
    /// Path of the **main executable**, not its directory. The directory is
    /// taken here, so callers pass the path they already have rather than
    /// remembering which of the two each field wants.
    pub executable: Option<&'a str>,
    /// Path of the image that records the dependency being expanded.
    pub loader: Option<&'a str>,
    /// `LC_RPATH` entries in search order — the loading image's own, then any
    /// inherited from the images that loaded it.
    pub rpaths: &'a [String],
}

/// Expand `install_name` into the paths to try, in search order.
///
/// A name with no `@` prefix expands to itself: it is already a path, whether
/// absolute or relative. A name whose prefix cannot be expanded — `@loader_path`
/// with no loader, `@rpath` with an empty stack — yields **nothing**, which is
/// the honest answer. Returning the unexpanded name as a candidate would send
/// the caller looking for a directory literally named `@rpath`, and turn a
/// missing-context bug into a missing-file error pointing at the wrong thing.
pub fn expand(install_name: &str, ctx: &PathContext<'_>) -> Vec<String> {
    let mut out = Vec::new();

    if let Some(rest) = strip_prefix_dir(install_name, RPATH) {
        for rpath in ctx.rpaths {
            // An `LC_RPATH` entry is itself expandable, and commonly is:
            // `@loader_path/../Frameworks` is the standard app-bundle spelling.
            for base in expand_base(rpath, ctx) {
                push_joined(&mut out, &base, rest);
                if out.len() >= MAX_CANDIDATES {
                    return out;
                }
            }
        }
        return out;
    }

    for base in expand_base(install_name, ctx) {
        out.push(base);
    }
    out
}

/// Expand a path that may start with `@executable_path` or `@loader_path`, but
/// not `@rpath`. Used both for install names and for `LC_RPATH` entries.
fn expand_base(path: &str, ctx: &PathContext<'_>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(rest) = strip_prefix_dir(path, EXECUTABLE_PATH) {
        if let Some(exe) = ctx.executable {
            push_joined(&mut out, dirname(exe), rest);
        }
    } else if let Some(rest) = strip_prefix_dir(path, LOADER_PATH) {
        if let Some(loader) = ctx.loader {
            push_joined(&mut out, dirname(loader), rest);
        }
    } else {
        out.push(path.to_string());
    }
    out
}

/// Strip `prefix` when it is followed by `/` or is the whole string.
///
/// The `/`-or-end check is what stops `@rpathological/lib.dylib` from being
/// treated as an `@rpath` reference — a prefix match alone would.
fn strip_prefix_dir<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(prefix)?;
    match rest.as_bytes().first() {
        None => Some(""),
        Some(b'/') => Some(&rest[1..]),
        _ => None,
    }
}

fn push_joined(out: &mut Vec<String>, base: &str, rest: &str) {
    out.push(join(base, rest));
}

/// Join two path components with exactly one separator.
pub fn join(base: &str, rest: &str) -> String {
    if rest.is_empty() {
        return base.to_string();
    }
    if base.is_empty() {
        return rest.to_string();
    }
    let mut s = String::with_capacity(base.len() + 1 + rest.len());
    s.push_str(base.trim_end_matches('/'));
    s.push('/');
    s.push_str(rest);
    s
}

/// The directory containing `path`.
///
/// Returns `"."` for a bare filename and `"/"` for a root-level path, matching
/// the `dirname(3)` convention the linker's own documentation assumes.
pub fn dirname(path: &str) -> &str {
    match path.rfind('/') {
        None => ".",
        Some(0) => "/",
        Some(i) => &path[..i],
    }
}

/// Whether an install name refers to the OS's own libraries.
///
/// These do **not** exist as files on modern macOS — they live only in the dyld
/// shared cache — so a filesystem source must decline them and let a
/// [`SymbolProvider`](crate::resolve::SymbolProvider) answer instead. A source
/// that tried to open them would either fail confusingly or, worse, find a
/// stale copy left over from an older OS.
pub fn is_system_path(install_name: &str) -> bool {
    install_name.starts_with("/usr/lib/")
        || install_name.starts_with("/System/")
        || install_name.starts_with("/Library/Apple/")
}

/// Whether an install name needs a [`PathContext`] before it means anything.
pub fn is_relative_to_context(install_name: &str) -> bool {
    strip_prefix_dir(install_name, RPATH).is_some()
        || strip_prefix_dir(install_name, EXECUTABLE_PATH).is_some()
        || strip_prefix_dir(install_name, LOADER_PATH).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn rp(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|s| s.to_string()).collect()
    }

    /// Table-driven, because the interesting content here is the *cases*, and a
    /// table makes an omission visible in a way scattered assertions do not.
    #[test]
    fn expansion_table() {
        let rpaths = rp(&["@loader_path/../Frameworks", "/opt/lib", "@executable_path/lib"]);
        let ctx = PathContext {
            executable: Some("/Apps/Demo.app/Contents/MacOS/Demo"),
            loader: Some("/Apps/Demo.app/Contents/Frameworks/Core.framework/Core"),
            rpaths: &rpaths,
        };

        let cases: &[(&str, &[&str])] = &[
            // Plain paths pass through untouched, absolute or not.
            ("/usr/lib/libSystem.B.dylib", &["/usr/lib/libSystem.B.dylib"]),
            ("libfoo.dylib", &["libfoo.dylib"]),
            ("../lib/libfoo.dylib", &["../lib/libfoo.dylib"]),

            // @executable_path is the executable's directory, regardless of
            // which image recorded the dependency.
            (
                "@executable_path/../Frameworks/A.dylib",
                &["/Apps/Demo.app/Contents/MacOS/../Frameworks/A.dylib"],
            ),

            // @loader_path is the *recording* image's directory, which is a
            // different answer for the same process.
            (
                "@loader_path/libprivate.dylib",
                &["/Apps/Demo.app/Contents/Frameworks/Core.framework/libprivate.dylib"],
            ),

            // @rpath is a search: one candidate per rpath entry, in order, with
            // each entry itself expanded first.
            (
                "@rpath/libmath.dylib",
                &[
                    "/Apps/Demo.app/Contents/Frameworks/Core.framework/../Frameworks/libmath.dylib",
                    "/opt/lib/libmath.dylib",
                    "/Apps/Demo.app/Contents/MacOS/lib/libmath.dylib",
                ],
            ),
            (
                "@rpath/Sub.framework/Sub",
                &[
                    "/Apps/Demo.app/Contents/Frameworks/Core.framework/../Frameworks/Sub.framework/Sub",
                    "/opt/lib/Sub.framework/Sub",
                    "/Apps/Demo.app/Contents/MacOS/lib/Sub.framework/Sub",
                ],
            ),

            // A prefix that merely *starts with* a magic token is not one.
            ("@rpathological/lib.dylib", &["@rpathological/lib.dylib"]),
            ("@loader_pathx/lib.dylib", &["@loader_pathx/lib.dylib"]),
            ("@executable_pathing", &["@executable_pathing"]),

            // A bare token with nothing after it expands to the directory.
            ("@loader_path", &["/Apps/Demo.app/Contents/Frameworks/Core.framework"]),
            ("@executable_path", &["/Apps/Demo.app/Contents/MacOS"]),

            // Not a magic token at all.
            ("@unknown/lib.dylib", &["@unknown/lib.dylib"]),
            ("", &[""]),
        ];

        for (input, want) in cases {
            let got = expand(input, &ctx);
            let want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
            assert_eq!(got, want, "expanding {input:?}");
        }
    }

    #[test]
    fn missing_context_yields_no_candidates_rather_than_a_literal_at_sign() {
        let empty = PathContext::default();
        assert!(expand("@loader_path/lib.dylib", &empty).is_empty());
        assert!(expand("@executable_path/lib.dylib", &empty).is_empty());
        assert!(expand("@rpath/lib.dylib", &empty).is_empty());
        // A plain path still works without any context at all.
        assert_eq!(expand("/usr/lib/x.dylib", &empty), vec!["/usr/lib/x.dylib".to_string()]);
    }

    #[test]
    fn an_rpath_entry_needing_context_it_lacks_is_skipped_not_emitted() {
        // The second entry is usable, the first is not. Dropping the first and
        // keeping the second is the behaviour that lets a partially-specified
        // context still find the library.
        let rpaths = rp(&["@executable_path/lib", "/opt/lib"]);
        let ctx = PathContext { executable: None, loader: None, rpaths: &rpaths };
        assert_eq!(expand("@rpath/libz.dylib", &ctx), vec!["/opt/lib/libz.dylib".to_string()]);
    }

    #[test]
    fn dirname_matches_the_conventional_definition() {
        assert_eq!(dirname("/a/b/c"), "/a/b");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(dirname("a"), ".");
        assert_eq!(dirname(""), ".");
        assert_eq!(dirname("/"), "/");
        assert_eq!(dirname("a/b"), "a");
    }

    #[test]
    fn join_never_doubles_or_drops_a_separator() {
        assert_eq!(join("/a", "b"), "/a/b");
        assert_eq!(join("/a/", "b"), "/a/b");
        assert_eq!(join("/a//", "b"), "/a/b");
        assert_eq!(join("/a", ""), "/a");
        assert_eq!(join("", "b"), "b");
        assert_eq!(join("/", "b"), "/b");
    }

    #[test]
    fn system_paths_are_recognised() {
        assert!(is_system_path("/usr/lib/libSystem.B.dylib"));
        assert!(is_system_path("/System/Library/Frameworks/Foundation.framework/Foundation"));
        assert!(!is_system_path("/usr/local/lib/libz.dylib"));
        assert!(!is_system_path("@rpath/libmath.dylib"));
        assert!(!is_system_path("/Users/me/libmine.dylib"));
    }

    #[test]
    fn context_relativity_is_detected() {
        assert!(is_relative_to_context("@rpath/x"));
        assert!(is_relative_to_context("@loader_path/x"));
        assert!(is_relative_to_context("@executable_path"));
        assert!(!is_relative_to_context("@rpathological/x"));
        assert!(!is_relative_to_context("/usr/lib/x"));
    }

    #[test]
    fn a_pathological_rpath_stack_is_bounded() {
        let rpaths: Vec<String> = (0..MAX_CANDIDATES * 2).map(|i| alloc::format!("/p{i}")).collect();
        let ctx = PathContext { executable: None, loader: None, rpaths: &rpaths };
        assert_eq!(expand("@rpath/x.dylib", &ctx).len(), MAX_CANDIDATES);
    }
}
