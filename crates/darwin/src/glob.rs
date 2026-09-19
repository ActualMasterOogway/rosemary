#![cfg(not(target_os = "macos"))]

use std::ffi::{c_char, c_int, c_void};

type ErrorFn = unsafe extern "C" fn(*const c_char, c_int) -> c_int;

/// Darwin's public `glob_t`.
///
/// Six trailing pointers, not five: `gl_errfunc` precedes the five `GLOB_ALTDIRFUNC`
/// hooks (`gl_closedir`, `gl_readdir`, `gl_opendir`, `gl_lstat`, `gl_stat`). glibc's
/// has no `gl_errfunc` field - it keeps the callback on its stack - so the two
/// reserved areas are different lengths as well as different orders.
#[repr(C)]
struct Glob {
    pub gl_pathc: usize,
    pub gl_matchc: c_int,
    pub gl_offs: usize,
    pub gl_flags: c_int,
    pub gl_pathv: *mut *mut c_char,
    reserved: [*mut c_void; 6],
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<Glob>() == 88);

mod darwin {
    #![allow(dead_code)]

    use std::ffi::c_int;

    pub const APPEND: c_int = 0x0001;
    pub const DOOFFS: c_int = 0x0002;
    pub const ERR: c_int = 0x0004;
    pub const MARK: c_int = 0x0008;
    pub const NOCHECK: c_int = 0x0010;
    pub const NOSORT: c_int = 0x0020;
    pub const ALTDIRFUNC: c_int = 0x0040;
    pub const BRACE: c_int = 0x0080;
    pub const MAGCHAR: c_int = 0x0100;
    pub const NOMAGIC: c_int = 0x0200;
    pub const TILDE: c_int = 0x0800;
    pub const NOESCAPE: c_int = 0x2000;

    pub const NOSPACE: c_int = -1;
    pub const ABORTED: c_int = -2;
    pub const NOMATCH: c_int = -3;
}

/// glibc's `GLOB_*`. `libc` declares no `GLOB_` constant for any Linux target, so
/// these are written out rather than borrowed.
#[cfg(target_os = "linux")]
mod host {
    use std::ffi::c_int;

    pub const MAGCHAR: c_int = 1 << 8;

    /// `GLOB_NOSPACE` is 1, `GLOB_ABORTED` 2, `GLOB_NOMATCH` 3 and `GLOB_NOSYS` 4;
    /// Darwin spells the same four -1 through -4. Negating is the whole mapping.
    pub const LAST_ERROR: c_int = 4;
}

/// The input flags that have a counterpart, Darwin's paired with glibc's.
///
/// Deliberately absent: `GLOB_ALTDIRFUNC`, refused outright below;
/// `GLOB_MAGCHAR`, which is set on output rather than read on input;
/// `GLOB_QUOTE`, a historical no-op Darwin ignores too; and `GLOB_LIMIT`,
/// whose behaviour glibc has unconditionally.
#[cfg(target_os = "linux")]
const FLAGS: &[(c_int, c_int)] = &[
    (darwin::APPEND, libc::GLOB_APPEND),
    (darwin::DOOFFS, libc::GLOB_DOOFFS),
    (darwin::ERR, libc::GLOB_ERR),
    (darwin::MARK, libc::GLOB_MARK),
    (darwin::NOCHECK, libc::GLOB_NOCHECK),
    (darwin::NOSORT, libc::GLOB_NOSORT),
    (darwin::BRACE, libc::GLOB_BRACE),
    (darwin::NOMAGIC, libc::GLOB_NOMAGIC),
    (darwin::TILDE, libc::GLOB_TILDE),
    (darwin::NOESCAPE, libc::GLOB_NOESCAPE),
];

/// glibc `glob64_t`
/// 
/// Necessary for re-declaration because the musl libc crate doesn't export this
/// even though musl has it.
/// 
/// TODO: completely reimplement glob64/glob rather than rely on the host impl because this
/// translation is far too complex to justify anymore.
#[cfg(target_os = "linux")]
#[repr(C)]
struct HostGlob {
    pathc: usize,
    pathv: *mut *mut c_char,
    offs: usize,
    flags: c_int,
    reserved: [*mut c_void; 5],
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
const _: () = {
    use std::mem::offset_of;

    assert!(size_of::<HostGlob>() == size_of::<libc::glob64_t>());
    assert!(align_of::<HostGlob>() == align_of::<libc::glob64_t>());
    assert!(offset_of!(HostGlob, pathc) == offset_of!(libc::glob64_t, gl_pathc));
    assert!(offset_of!(HostGlob, pathv) == offset_of!(libc::glob64_t, gl_pathv));
    assert!(offset_of!(HostGlob, offs) == offset_of!(libc::glob64_t, gl_offs));
    assert!(offset_of!(HostGlob, flags) == offset_of!(libc::glob64_t, gl_flags));
};

#[cfg(target_os = "linux")]
impl HostGlob {
    /// An empty one, for the fields glibc reads only under a flag this never sets.
    fn empty() -> Self {
        Self {
            pathc: 0,
            pathv: std::ptr::null_mut(),
            offs: 0,
            flags: 0,
            // `gl_closedir` through `gl_stat`, read only under `GLOB_ALTDIRFUNC`,
            // which never reaches the host - see `glob`.
            reserved: [std::ptr::null_mut(); 5],
        }
    }
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    // See the module note: the guest asked for `glob$INODE64`, whose counterpart
    // here is `glob64`. musl has no separate narrow form and aliases the two.
    #[cfg_attr(target_env = "gnu", link_name = "glob64")]
    #[cfg_attr(not(target_env = "gnu"), link_name = "glob")]
    fn host_glob(
        pattern: *const c_char,
        flags: c_int,
        err: Option<ErrorFn>,
        out: *mut HostGlob,
    ) -> c_int;

    #[cfg_attr(target_env = "gnu", link_name = "globfree64")]
    #[cfg_attr(not(target_env = "gnu"), link_name = "globfree")]
    fn host_globfree(out: *mut HostGlob);
}

/// `int glob(const char *pattern, int flags, int (*errfunc)(const char *, int), glob_t *pglob)`
///
/// # Safety
///
/// `pattern` is a C string, `out` points at a writable Darwin `glob_t` - already
/// filled by a previous call when `GLOB_APPEND` is set - and `err`, if given, is
/// callable. That is `glob`'s own contract.
#[cfg(target_os = "linux")]
unsafe extern "C" fn glob(
    pattern: *const c_char,
    flags: c_int,
    err: Option<ErrorFn>,
    out: *mut Glob,
) -> c_int {
    // `GLOB_ALTDIRFUNC` replaces the directory walk with the caller's own
    // `gl_opendir`/`gl_readdir`/`gl_stat`. Three things are wrong with forwarding
    // it: the hooks sit at different offsets in the two structures, `gl_readdir`
    // would have to return a *host* `struct dirent` - the divergence
    // `crate::divergent` refuses the whole directory family over - and `gl_stat`
    // a host `struct stat`. Refusing is the honest answer; mapping the bit and
    // letting glibc walk the real filesystem instead would silently ignore the
    // caller's hooks.
    if flags & darwin::ALTDIRFUNC != 0 {
        return darwin::ABORTED;
    }

    let host_flags = FLAGS.iter().fold(0, |mapped, (darwin, host)| {
        mapped | if flags & darwin != 0 { *host } else { 0 }
    });
    let appending = flags & darwin::APPEND != 0;

    let mut request = HostGlob::empty();
    if appending {
        // SAFETY: `GLOB_APPEND` is the caller's promise that `out` holds the
        // result of a previous call, so these three are initialised.
        unsafe {
            request.pathc = (*out).gl_pathc;
            request.pathv = (*out).gl_pathv;
        }
    }
    if flags & (darwin::DOOFFS | darwin::APPEND) != 0 {
        // Only those two flags give `gl_offs` a defined value; without either,
        // a caller need never have written the field.
        request.offs = unsafe { (*out).gl_offs };
    }
    let previous = request.pathc;

    // `err` is guest code, entered by the host's `glob` during the call - so the
    // boundary `crate::callback` raises around a comparator is needed here too.
    let status =
        unsafe { crate::call_as_darwin!(host_glob(pattern, host_flags, err, &raw mut request)) };

    unsafe {
        (*out).gl_pathc = request.pathc;
        // Darwin zeroes this at entry and counts only the paths *this* call added.
        (*out).gl_matchc = request.pathc.saturating_sub(previous) as c_int;
        (*out).gl_offs = request.offs;
        (*out).gl_pathv = request.pathv;
        // Both sides report a pattern that held metacharacters by setting
        // `GLOB_MAGCHAR` on the way out. Nothing else about `gl_flags` comes from
        // the host: glibc's other bits mean different things at these positions.
        (*out).gl_flags = (flags & !darwin::MAGCHAR)
            | if request.flags & host::MAGCHAR != 0 {
                darwin::MAGCHAR
            } else {
                0
            };
    }

    // The caller's `gl_errfunc` and `GLOB_ALTDIRFUNC` hooks live in `reserved` and
    // are left exactly as they were: they are the *guest's* pointers, and glibc
    // neither read nor wrote them.
    match status {
        1..=host::LAST_ERROR => -status,
        other => other,
    }
}

#[cfg(target_os = "linux")]
unsafe extern "C" fn globfree(out: *mut Glob) {
    let mut request = HostGlob::empty();
    unsafe {
        request.pathc = (*out).gl_pathc;
        request.pathv = (*out).gl_pathv;
        request.offs = (*out).gl_offs;
    }
    // A null `gl_pathv` is the documented "nothing to free", which the host
    // checks itself - so a never-globbed, zeroed structure is safe to pass.
    unsafe { host_globfree(&raw mut request) };
    // The host nulls `gl_pathv`, and Darwin's does the same. Leaving the guest's
    // copy pointing at freed memory is how a double `globfree` becomes a double
    // free.
    unsafe { (*out).gl_pathv = request.pathv };
}

// The remaining non-Darwin hosts are the BSDs, whose `glob_t` matches Darwin's
// field for field. Not quite bit for bit - `gl_matchc` is a `size_t` on FreeBSD
// and an `int` here - so this is a placeholder for a host nothing builds for yet
// rather than a checked translation.
#[cfg(not(target_os = "linux"))]
unsafe extern "C" fn glob(
    pattern: *const c_char,
    flags: c_int,
    err: Option<ErrorFn>,
    out: *mut Glob,
) -> c_int {
    unsafe { libc::glob(pattern, flags, err, out.cast()) }
}

#[cfg(not(target_os = "linux"))]
unsafe extern "C" fn globfree(out: *mut Glob) {
    unsafe { libc::globfree(out.cast()) }
}

exports::exports! {
    functions {
        "glob" => glob,
        "globfree" => globfree,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, CString};

    /// A directory of three files, named per test so the suite can run in
    /// parallel.
    fn fixture(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("r3-glob-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        for name in ["beta", "alpha", "gamma"] {
            std::fs::write(dir.join(name), b"x").expect("a file");
        }
        dir
    }

    fn pattern(dir: &std::path::Path, tail: &str) -> CString {
        CString::new(format!("{}/{tail}", dir.display())).expect("no interior NUL")
    }

    fn zeroed() -> Glob {
        Glob {
            gl_pathc: 0,
            gl_matchc: 0,
            gl_offs: 0,
            gl_flags: 0,
            gl_pathv: std::ptr::null_mut(),
            reserved: [std::ptr::null_mut(); 6],
        }
    }

    /// The matched paths, as basenames - the slots `gl_offs` reserves are not
    /// matches and are checked separately.
    fn matched(found: &Glob) -> Vec<String> {
        (0..found.gl_pathc)
            .map(|i| {
                // SAFETY: `glob` reported `gl_pathc` paths from `gl_offs` on.
                let path = unsafe { *found.gl_pathv.add(found.gl_offs + i) };
                assert!(!path.is_null());
                let full = unsafe { CStr::from_ptr(path) }.to_string_lossy();
                full.rsplit('/').next().expect("a basename").to_owned()
            })
            .collect()
    }

    #[test]
    fn matches_every_file_and_frees_the_vector() {
        let dir = fixture("all");
        let mut found = zeroed();
        // SAFETY: a live pattern and structure, freed before the test ends.
        unsafe {
            assert_eq!(
                glob(pattern(&dir, "*").as_ptr(), 0, None, &raw mut found),
                0
            );
            assert_eq!(found.gl_pathc, 3);
            assert_eq!(found.gl_matchc, 3);
            // Sorted by default, which is the difference `GLOB_NOSORT` makes.
            assert_eq!(matched(&found), ["alpha", "beta", "gamma"]);
            globfree(&raw mut found);
        }
        // Nulled on the way out, so a second `globfree` is not a double free.
        assert!(found.gl_pathv.is_null());
        // SAFETY: a structure whose `gl_pathv` is null, which is the documented
        // "nothing to free".
        unsafe { globfree(&raw mut found) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_counts_only_this_calls_matches() {
        let dir = fixture("append");
        let mut found = zeroed();
        // SAFETY: as above; the second call is told the first one's result.
        unsafe {
            assert_eq!(
                glob(pattern(&dir, "alpha").as_ptr(), 0, None, &raw mut found),
                0
            );
            assert_eq!((found.gl_pathc, found.gl_matchc), (1, 1));
            assert_eq!(
                glob(
                    pattern(&dir, "[bg]*").as_ptr(),
                    darwin::APPEND,
                    None,
                    &raw mut found
                ),
                0
            );
            // Three paths in the vector, two of them added by this call - which
            // is the whole difference between Darwin's two counters.
            assert_eq!(found.gl_pathc, 3);
            assert_eq!(found.gl_matchc, 2);
            assert_eq!(matched(&found), ["alpha", "beta", "gamma"]);
            globfree(&raw mut found);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unmatched_pattern_is_darwins_negative_code() {
        let dir = fixture("nomatch");
        let mut found = zeroed();
        // SAFETY: a live pattern and structure; nothing is allocated on failure.
        let status = unsafe { glob(pattern(&dir, "delta*").as_ptr(), 0, None, &raw mut found) };
        // glibc answers 3 and Darwin -3. Getting this wrong is invisible to a
        // guest that only tests for zero and catastrophic for one that does not.
        assert_eq!(status, darwin::NOMATCH);
        assert_eq!(status, -3);
        assert!(darwin::NOSPACE == -1 && darwin::ABORTED == -2);
        // SAFETY: nothing was allocated, so `gl_pathv` is null.
        unsafe { globfree(&raw mut found) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nocheck_answers_with_the_pattern_itself() {
        let dir = fixture("nocheck");
        let mut found = zeroed();
        // SAFETY: as above.
        unsafe {
            assert_eq!(
                glob(
                    pattern(&dir, "delta*").as_ptr(),
                    darwin::NOCHECK,
                    None,
                    &raw mut found
                ),
                0
            );
            assert_eq!(matched(&found), ["delta*"]);
            globfree(&raw mut found);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dooffs_reserves_leading_slots() {
        let dir = fixture("dooffs");
        let mut found = zeroed();
        found.gl_offs = 2;
        // SAFETY: as above.
        unsafe {
            assert_eq!(
                glob(
                    pattern(&dir, "*").as_ptr(),
                    darwin::DOOFFS,
                    None,
                    &raw mut found
                ),
                0
            );
            // `gl_offs` is not folded into `gl_pathc` on either side: the count is
            // of matches, and the reserved slots sit in front of them.
            assert_eq!(found.gl_pathc, 3);
            assert_eq!(found.gl_offs, 2);
            assert!((*found.gl_pathv).is_null() && (*found.gl_pathv.add(1)).is_null());
            assert_eq!(matched(&found), ["alpha", "beta", "gamma"]);
            globfree(&raw mut found);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both bits are assigned different values on the two sides, and dropping
    /// either one silently answers `GLOB_NOMATCH` for a pattern that matches.
    #[test]
    fn brace_is_translated_rather_than_dropped() {
        let dir = fixture("brace");
        let mut found = zeroed();
        // SAFETY: as above.
        unsafe {
            let braced = pattern(&dir, "{alpha,gamma}");
            // Without the flag the braces are literal, and no such file exists.
            assert_eq!(
                glob(braced.as_ptr(), 0, None, &raw mut found),
                darwin::NOMATCH
            );
            assert_eq!(
                glob(braced.as_ptr(), darwin::BRACE, None, &raw mut found),
                0
            );
            assert_eq!(matched(&found), ["alpha", "gamma"]);
            globfree(&raw mut found);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tilde_is_translated_rather_than_dropped() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        let mut found = zeroed();
        // SAFETY: a static pattern and a live structure.
        unsafe {
            assert_eq!(glob(c"~".as_ptr(), darwin::TILDE, None, &raw mut found), 0);
            assert_eq!(found.gl_pathc, 1);
            let first = CStr::from_ptr(*found.gl_pathv)
                .to_string_lossy()
                .into_owned();
            assert_eq!(first, home);
            globfree(&raw mut found);
        }
    }

    /// The hooks are the guest's, they sit at different offsets on each side, and
    /// `gl_readdir` would have to hand back a host `struct dirent`. Refusing is
    /// the answer; walking the real directory instead would ignore them silently.
    #[test]
    fn altdirfunc_is_refused_rather_than_ignored() {
        let dir = fixture("altdirfunc");
        let mut found = zeroed();
        // SAFETY: as above.
        let status = unsafe {
            glob(
                pattern(&dir, "*").as_ptr(),
                darwin::ALTDIRFUNC,
                None,
                &raw mut found,
            )
        };
        assert_eq!(status, darwin::ABORTED);
        assert!(found.gl_pathv.is_null());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gl_flags_reports_the_input_flags_and_magchar() {
        let dir = fixture("flags");
        let mut found = zeroed();
        // SAFETY: as above.
        unsafe {
            assert_eq!(
                glob(
                    pattern(&dir, "*").as_ptr(),
                    darwin::NOSORT,
                    None,
                    &raw mut found
                ),
                0
            );
            // The caller's own flags come back in Darwin's spelling, not the
            // host's - `GLOB_NOSORT` is 0x20 here and 4 there.
            assert_eq!(found.gl_flags & darwin::NOSORT, darwin::NOSORT);
            assert_eq!(found.gl_flags & darwin::MARK, 0);
            // And the one bit the call adds: the pattern held a metacharacter.
            assert_eq!(found.gl_flags & darwin::MAGCHAR, darwin::MAGCHAR);
            globfree(&raw mut found);

            let mut plain = zeroed();
            assert_eq!(
                glob(pattern(&dir, "alpha").as_ptr(), 0, None, &raw mut plain),
                0
            );
            assert_eq!(plain.gl_flags & darwin::MAGCHAR, 0);
            globfree(&raw mut plain);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn both_entries_are_published() {
        for name in ["glob", "globfree"] {
            assert!(symbol(name).is_some(), "{name}");
        }
    }
}
